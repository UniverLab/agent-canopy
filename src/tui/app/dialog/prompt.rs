use anyhow::Result;
use ratatui::style::Color;
use std::collections::{HashMap, HashSet};

use super::at_picker::AtPicker;
use crate::db::Database;
use crate::domain::project::Project;
use crate::tui::app::types::Focus;
use std::path::Path;

/// Picker state for adding/removing sections
#[derive(Debug, Clone, PartialEq, Default)]
pub enum SectionPickerMode {
    #[default]
    None,
    AddSection {
        selected: usize,
    },
    RemoveSection {
        selected: usize,
    },
    AddCustom {
        input: String,
    },
    /// Skills picker for the Tools section — entries are `(label, raw_name, prefix)`
    SkillsPicker {
        selected: usize,
        /// `(display_label, raw_name, prefix)` — `prefix` is "skill" or "global"
        entries: Vec<(String, String, String)>,
        /// `None` → create a new tools section on confirm; `Some(id)` → replace content of that section
        replace_id: Option<String>,
    },
    ProjectPicker {
        selected: usize,
        entries: Vec<ProjectPickerEntry>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectPickerEntry {
    pub hash: String,
    pub name: String,
    pub path: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RagScope<'a> {
    Global,
    Project(&'a str),
}

/// New simplified prompt template dialog with dynamic sections
/// Now supports multiple instances of the same section type
pub struct SimplePromptDialog {
    /// Map of unique section IDs to their content
    pub sections: HashMap<String, String>,
    /// Ordered list of section IDs currently enabled
    pub enabled_sections: Vec<String>,
    /// Which section field is currently focused
    pub focused_section: usize,
    /// Previous focus before opening the dialog
    pub prev_focus: Option<Focus>,
    /// State for the section picker modal
    pub picker_mode: SectionPickerMode,
    /// Counter for generating unique IDs per section type
    pub section_counters: HashMap<String, usize>,
    /// Per-section cursor positions (char index)
    pub section_cursors: HashMap<String, usize>,
    /// Per-section scroll offsets (visual line)
    pub section_scrolls: HashMap<String, usize>,
    /// Active `@`-file picker (inline dropdown), if open.
    pub at_picker: Option<AtPicker>,
    /// Collapsed paste content: placeholder text is stored in `sections`,
    /// the real pasted content lives here and is used for building the prompt.
    pub collapsed_pastes: HashMap<String, String>,
    /// Section IDs that are read-only (auto-filled, cannot be edited).
    pub locked_sections: HashSet<String>,
    /// Invisible system block appended at the bottom of the final prompt.
    /// None = don't append. Set once per workdir session (idempotent).
    pub system_content: Option<String>,
}

impl SimplePromptDialog {
    pub fn new() -> Self {
        let mut counters = HashMap::new();
        counters.insert("instruction".to_string(), 2usize);
        counters.insert("context".to_string(), 2usize);
        let mut cursors = HashMap::new();
        cursors.insert("instruction_1".to_string(), 0usize);
        let mut scrolls = HashMap::new();
        scrolls.insert("instruction_1".to_string(), 0usize);
        let mut sections = HashMap::new();
        sections.insert("instruction_1".to_string(), String::new());
        Self {
            sections,
            enabled_sections: vec!["instruction_1".to_string()],
            focused_section: 0,
            prev_focus: None,
            picker_mode: SectionPickerMode::None,
            section_counters: counters,
            section_cursors: cursors,
            section_scrolls: scrolls,
            at_picker: None,
            collapsed_pastes: HashMap::new(),
            locked_sections: HashSet::new(),
            system_content: None,
        }
    }

    /// Get cursor position for a section
    pub fn cursor(&self, section: &str) -> usize {
        self.section_cursors.get(section).copied().unwrap_or(0)
    }

    /// Get scroll offset for a section
    pub fn scroll(&self, section: &str) -> usize {
        self.section_scrolls.get(section).copied().unwrap_or(0)
    }

    /// Returns true if the section is locked (read-only).
    pub fn is_locked(&self, section_id: &str) -> bool {
        self.locked_sections.contains(section_id)
    }

    /// Mark a section as locked (read-only).
    pub fn lock_section(&mut self, section_id: &str) {
        self.locked_sections.insert(section_id.to_string());
    }

    /// Generate unique ID for a section instance (always uses `name_N` format, N starting at 1).
    fn generate_section_id(&mut self, section_name: &str) -> String {
        let counter = self
            .section_counters
            .entry(section_name.to_string())
            .or_insert(1);
        let id = format!("{}_{}", section_name, counter);
        *counter += 1;
        id
    }

    fn section_type(section_id: &str) -> &str {
        Self::get_available_sections()
            .into_iter()
            .map(|(name, _)| name)
            .find(|name| section_id == *name || section_id.starts_with(&format!("{name}_")))
            .unwrap_or(section_id)
    }

    fn section_matches_prefix(section_id: &str, prefix: &str) -> bool {
        section_id == prefix || section_id.starts_with(&format!("{prefix}_"))
    }

    fn instruction_count(&self) -> usize {
        self.enabled_sections
            .iter()
            .filter(|section_id| Self::section_matches_prefix(section_id, "instruction"))
            .count()
    }

    fn insert_section(&mut self, section_name: &str, content: String) -> String {
        let unique_id = self.generate_section_id(section_name);
        let cursor_pos = content.chars().count();
        self.enabled_sections.push(unique_id.clone());
        self.sections.insert(unique_id.clone(), content);
        self.section_cursors.insert(unique_id.clone(), cursor_pos);
        self.section_scrolls.insert(unique_id.clone(), 0);
        self.collapsed_pastes.remove(&unique_id);
        self.focused_section = self.enabled_sections.len() - 1;
        unique_id
    }

    /// Add a section instance (can be same type multiple times)
    pub fn add_section(&mut self, section_name: &str) {
        self.insert_section(section_name, String::new());
    }

    /// Add a section with pre-existing content (used for context transfer and initial content).
    /// Returns the generated section ID.
    pub fn add_section_with_content(&mut self, section_name: &str, content: String) -> String {
        self.insert_section(section_name, content)
    }

    /// Remove a specific section instance.
    /// The last remaining instruction section cannot be removed.
    pub fn remove_section(&mut self, section_id: &str) {
        if Self::section_matches_prefix(section_id, "instruction") && self.instruction_count() <= 1
        {
            return;
        }

        self.enabled_sections.retain(|s| s != section_id);
        self.sections.remove(section_id);
        self.section_cursors.remove(section_id);
        self.section_scrolls.remove(section_id);
        self.collapsed_pastes.remove(section_id);
        if self.focused_section > 0 {
            self.focused_section = self.focused_section.saturating_sub(1);
        }
    }

    /// Get available section types (these can always be added again).
    /// RAG Search is only included when an embeddings model is configured.
    pub fn get_available_sections() -> Vec<(&'static str, &'static str)> {
        let rag_enabled = dirs::home_dir()
            .map(|h| {
                let config = crate::domain::canopy_config::CanopyConfig::load(&h.join(".canopy"));
                !config.embeddings_model.trim().is_empty()
            })
            .unwrap_or(false);

        let mut sections = vec![
            ("instruction", "Instruction"),
            ("context", "Context"),
            ("project_context", "Project Context"),
            ("resources", "Resources"),
        ];
        if rag_enabled {
            sections.push(("rag_search", "RAG Search"));
        }
        sections.extend([("constraints", "Constraints"), ("tools", "Tools")]);
        sections
    }

    /// Return true if this section ID represents the read-only "tools" section.
    pub fn is_tools_section(section_id: &str) -> bool {
        section_id == "tools" || section_id.starts_with("tools_")
    }

    /// Collect all available skills for the skills picker.
    /// Returns `Vec<(display_label, raw_name, prefix)>`.
    pub fn collect_skills_for_picker(workdir: &std::path::Path) -> Vec<(String, String, String)> {
        let mut entries: Vec<(String, String, String)> = Vec::new();
        let project = workdir.join(".agents").join("skills");
        add_skills_from_dir(&project, "skill", &mut entries);
        if let Some(global) = dirs::home_dir().map(|h| h.join(".agents").join("skills")) {
            if global != project {
                add_skills_from_dir(&global, "global", &mut entries);
            }
        }
        entries
    }

    pub fn collect_projects_for_picker(db: &Database) -> Result<Vec<ProjectPickerEntry>> {
        let mut entries = db
            .list_projects()?
            .into_iter()
            .map(|project| ProjectPickerEntry {
                hash: project.hash,
                name: project.name,
                path: project.path,
            })
            .collect::<Vec<_>>();
        entries.sort_by(|left, right| left.name.cmp(&right.name).then(left.path.cmp(&right.path)));
        Ok(entries)
    }

    /// Set the content of a specific tools section to a single skill label.
    /// Used by the SkillsPicker to replace the skill in an existing tools section.
    pub fn set_tools_section_skill(&mut self, section_id: &str, label: &str) {
        self.sections
            .insert(section_id.to_string(), label.to_string());
    }

    /// Get section types available to add (can always add more instances)
    pub fn get_addable_sections(&self) -> Vec<(&'static str, &'static str)> {
        Self::get_available_sections()
    }

    fn section_display_name(section_id: &str) -> String {
        let section_name = Self::section_type(section_id);
        let label = Self::get_available_sections()
            .into_iter()
            .find(|(name, _)| *name == section_name)
            .map(|(_, label)| label)
            .unwrap_or(section_name);

        if section_id.contains('_') {
            return format!("{} {}", label, section_id.rsplit('_').next().unwrap_or(""));
        }

        label.to_string()
    }

    /// Get section instances available to remove (last instruction is protected)
    pub fn get_removable_sections(&self) -> Vec<(String, String)> {
        let instruction_count = self.instruction_count();
        self.enabled_sections
            .iter()
            .filter(|section_id| {
                !Self::section_matches_prefix(section_id, "instruction") || instruction_count > 1
            })
            .map(|section_id| (section_id.clone(), Self::section_display_name(section_id)))
            .collect()
    }

    /// Get the content for a section
    pub fn get_section_content(&self, section_name: &str) -> String {
        self.sections.get(section_name).cloned().unwrap_or_default()
    }

    /// Set the content for a section
    pub fn set_section_content(&mut self, section_name: &str, content: String) {
        self.sections.insert(section_name.to_string(), content);
    }

    /// Get the real content for a section, resolving any collapsed paste.
    pub fn section_content_for_build(&self, section_id: &str) -> Option<&str> {
        self.collapsed_pastes
            .get(section_id)
            .map(|s| s.as_str())
            .or_else(|| self.sections.get(section_id).map(|s| s.as_str()))
    }

    fn section_entries<'a>(&'a self, prefix: &str) -> Vec<&'a str> {
        self.enabled_sections
            .iter()
            .filter(|section_id| Self::section_matches_prefix(section_id, prefix))
            .filter_map(|section_id| self.section_content_for_build(section_id))
            .map(str::trim)
            .filter(|content| !content.is_empty())
            .collect()
    }

    fn section_lines(&self, prefix: &str) -> Vec<String> {
        self.section_entries(prefix)
            .into_iter()
            .flat_map(str::lines)
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .map(ToOwned::to_owned)
            .collect()
    }

    fn format_project_block(project: &Project) -> String {
        let mut lines = vec![
            format!("name: {}", project.name),
            format!("workdir_hash: {}", project.hash),
            format!("path: {}", project.path),
        ];
        if let Some(description) = project.description.as_deref() {
            lines.push(format!("description: {}", description));
        }
        if let Some(tags) = project.tags.as_deref() {
            lines.push(format!("tags: {}", tags));
        }
        if let Some(indexed_at) = project.indexed_at {
            lines.push(format!("indexed_at: {}", indexed_at));
        }
        lines.join("\n")
    }

    fn format_file_resource(path: &Path) -> String {
        format!("path: {}\nkind: file", path.display())
    }

    fn format_rag_chunk(query: &str, chunk: &crate::rag::vector_store::SearchResult) -> String {
        let dist = chunk
            .distance
            .map_or("—".to_string(), |d| format!("{d:.4}"));
        format!(
            "kind: rag_chunk\nquery: {query}\npath: {}\ndistance: {}\ncontent:\n{}",
            chunk.file_path, dist, chunk.content
        )
    }

    fn lookup_project_reference(db: &Database, entry: &str) -> Result<Option<Project>> {
        if let Some(project) = db.get_project(entry)? {
            return Ok(Some(project));
        }

        let path = Path::new(entry);
        if path.exists() {
            return db.get_project_by_path_or_ancestor(path);
        }

        Ok(None)
    }

    fn resolve_resource_entry(db: &Database, raw: &str) -> String {
        let trimmed = raw.trim();

        if trimmed.starts_with("http://") || trimmed.starts_with("https://") {
            return format!("path: {trimmed}\nkind: url");
        }

        let path = Path::new(trimmed);
        if path.exists() {
            if path.is_dir() {
                if let Ok(Some(project)) = db.get_project_by_path(path) {
                    return Self::format_project_block(&project);
                }
                return format!("path: {}\nkind: directory", path.display());
            }
            return Self::format_file_resource(path);
        }

        format!("content: {trimmed}\nkind: raw")
    }

    fn resolve_rag_scope<'a>(
        query: &'a str,
        default_project_hash: Option<&'a str>,
    ) -> (RagScope<'a>, &'a str) {
        if let Some(rest) = query.strip_prefix("global:") {
            return (RagScope::Global, rest.trim());
        }
        if let Some(rest) = query.strip_prefix("project:") {
            if let Some((project_hash, query)) = rest.split_once(':') {
                return (RagScope::Project(project_hash.trim()), query.trim());
            }
        }

        default_project_hash.map_or((RagScope::Global, query.trim()), |project_hash| {
            (RagScope::Project(project_hash), query.trim())
        })
    }

    fn default_project_hash(&self, db: &Database, current_workdir: &Path) -> Option<String> {
        db.get_project_by_path_or_ancestor(current_workdir)
            .ok()
            .flatten()
            .map(|project| project.hash)
    }

    fn resolve_project_contexts(&self, db: &Database) -> Vec<String> {
        let mut seen_hashes = HashSet::new();
        let mut projects = self
            .section_lines("project_context")
            .into_iter()
            .filter_map(|entry| Self::lookup_project_reference(db, &entry).ok().flatten())
            .filter(|project| seen_hashes.insert(project.hash.clone()))
            .collect::<Vec<_>>();

        for project in self.derived_project_contexts_from_resources(db) {
            if seen_hashes.insert(project.hash.clone()) {
                projects.push(project);
            }
        }

        projects
            .into_iter()
            .map(|project| Self::format_project_block(&project))
            .collect()
    }

    fn derived_project_contexts_from_resources(&self, db: &Database) -> Vec<Project> {
        self.section_lines("resources")
            .into_iter()
            .filter_map(|entry| {
                let path = Path::new(&entry);
                path.exists()
                    .then(|| db.get_project_by_path_or_ancestor(path).ok().flatten())
                    .flatten()
            })
            .collect()
    }

    fn resolve_resource_entries(&self, db: &Database) -> Vec<String> {
        self.section_lines("resources")
            .into_iter()
            .map(|entry| Self::resolve_resource_entry(db, &entry))
            .collect()
    }

    fn search_rag_resources<'a>(
        _db: &Database,
        query: &'a str,
        _default_project_hash: Option<&'a str>,
    ) -> Vec<String> {
        let (scope, resolved_query) = Self::resolve_rag_scope(query, _default_project_hash);
        let _ = scope;
        if resolved_query.is_empty() {
            return Vec::new();
        }

        let canopy_dir = match dirs::home_dir() {
            Some(h) => h.join(".canopy"),
            None => return Vec::new(),
        };
        let config = crate::domain::canopy_config::CanopyConfig::load(&canopy_dir);
        let model = config.embeddings_model.trim();
        if model.is_empty() {
            return Vec::new();
        }
        let Ok(dimensions) = crate::rag::embedding_client::model_dimensions(model) else {
            return Vec::new();
        };
        let Ok(rt) = tokio::runtime::Handle::try_current() else {
            return Vec::new();
        };

        rt.block_on(async {
            let Ok(store) = crate::rag::vector_store::VectorStore::new(dimensions).await else {
                return Vec::new();
            };
            let Ok(embedder) = crate::rag::embedding_client::client_from_config(&config) else {
                return Vec::new();
            };
            let Ok(query_vec) = embedder.embed(resolved_query) else {
                return Vec::new();
            };
            let Ok(results) = store.search_similar(&query_vec, 5).await else {
                return Vec::new();
            };
            results
                .iter()
                .map(|chunk| Self::format_rag_chunk(resolved_query, chunk))
                .collect()
        })
    }

    fn resolve_rag_resources(
        &self,
        db: &Database,
        default_project_hash: Option<&str>,
    ) -> Vec<String> {
        self.section_lines("rag_search")
            .into_iter()
            .flat_map(|query| Self::search_rag_resources(db, &query, default_project_hash))
            .collect()
    }

    fn append_prompt_section(
        &self,
        result: &mut String,
        prefix: &str,
        header: &str,
        outer_tag: &str,
        item_tag: &str,
    ) {
        build_xml_block(
            result,
            &self.enabled_sections,
            |section_id| Self::section_matches_prefix(section_id, prefix),
            |section_id| self.section_content_for_build(section_id),
            header,
            outer_tag,
            item_tag,
        );
    }

    fn append_instruction_section(&self, result: &mut String) {
        result.push_str("# [INSTRUCTIONS]: Execution Logic\n");
        result.push_str("<instruction_set>\n");
        for content in self.section_entries("instruction") {
            push_xml_item(result, "instruction", content);
        }
        result.push_str("</instruction_set>\n\n");
    }

    fn append_tools_section(&self, result: &mut String) {
        let tool_lines = self.collect_tool_lines();
        if tool_lines.is_empty() {
            return;
        }

        result.push_str("# [TOOLS]: Skills & Capabilities\n");
        result.push_str("<tools>\n");
        for tool_line in tool_lines {
            append_tool_skill(result, &tool_line);
        }
        result.push_str("</tools>\n\n");
    }

    fn collect_tool_lines(&self) -> Vec<String> {
        self.section_entries("tools")
            .into_iter()
            .flat_map(|content| {
                content
                    .lines()
                    .map(str::trim)
                    .filter(|line| !line.is_empty())
                    .map(str::to_string)
                    .collect::<Vec<_>>()
            })
            .collect()
    }

    pub fn build_prompt_with_resolved_resources(
        &self,
        db: &Database,
        current_workdir: &Path,
    ) -> Result<String> {
        let mut result = self.build_prompt()?;
        let project_contexts = self.resolve_project_contexts(db);
        let default_project_hash = self.default_project_hash(db, current_workdir);
        let mut resources = self.resolve_resource_entries(db);
        resources.extend(self.resolve_rag_resources(db, default_project_hash.as_deref()));

        if let Some(section) = format_indexed_xml_section(
            "# [PROJECT CONTEXT]: Registered Project Metadata\n",
            "project_context",
            "project",
            &project_contexts,
        ) {
            result = format!("{section}{result}");
        }

        if let Some(section) = format_indexed_xml_section(
            "# [RESOURCES]: Knowledge Base & Data\n",
            "resources",
            "resource",
            &resources,
        ) {
            result = strip_resources_section(&result);
            result.push_str(&section);
        }

        Ok(result)
    }

    /// Build the final prompt from the filled sections with structured format
    /// Supports multiple instances of each section type
    pub fn build_prompt(&self) -> Result<String> {
        let mut result = String::new();
        self.append_prompt_section(
            &mut result,
            "context",
            "# [CONTEXT]: Project Background\n",
            "context",
            "context",
        );
        self.append_instruction_section(&mut result);
        self.append_prompt_section(
            &mut result,
            "resources",
            "# [RESOURCES]: Knowledge Base & Data\n",
            "resources",
            "resource",
        );
        self.append_prompt_section(
            &mut result,
            "constraints",
            "# [CONSTRAINTS]: Behavioral Boundaries\n",
            "constraints",
            "constraint",
        );
        self.append_tools_section(&mut result);

        if let Some(system) = &self.system_content {
            result.push_str("<system>\n");
            result.push_str(system);
            result.push_str("\n</system>\n");
        }

        Ok(result)
    }

    pub fn migrate_legacy_sections(&mut self, current_project_path: Option<&str>) {
        let had_memory_context = self
            .enabled_sections
            .iter()
            .any(|section_id| Self::section_matches_prefix(section_id, "memory_context"));
        let obsolete_sections = self
            .enabled_sections
            .iter()
            .filter(|section_id| {
                Self::section_matches_prefix(section_id, "memory_context")
                    || Self::section_matches_prefix(section_id, "examples")
            })
            .cloned()
            .collect::<Vec<_>>();

        for section_id in obsolete_sections {
            self.remove_section(&section_id);
        }

        let has_project_context = self
            .enabled_sections
            .iter()
            .any(|section_id| Self::section_matches_prefix(section_id, "project_context"));
        if had_memory_context && !has_project_context {
            if let Some(project_path) = current_project_path {
                self.add_section_with_content("project_context", project_path.to_string());
            }
        }
        self.focused_section = self
            .focused_section
            .min(self.enabled_sections.len().saturating_sub(1));
    }

    fn set_content_and_cursor(
        &mut self,
        section_id: &str,
        content: String,
        cursor: usize,
        field_width: usize,
    ) {
        self.set_section_content(section_id, content);
        self.section_cursors.insert(section_id.to_string(), cursor);
        self.update_section_scroll(section_id, field_width);
    }

    fn split_content_at_cursor(&self, section_id: &str) -> (String, String, usize) {
        let content = self.get_section_content(section_id);
        let chars: Vec<char> = content.chars().collect();
        let cursor = self.cursor(section_id).min(chars.len());
        let before = chars[..cursor].iter().collect();
        let after = chars[cursor..].iter().collect();
        (before, after, cursor)
    }

    fn replace_char_range(
        &mut self,
        section_id: &str,
        start: usize,
        end: usize,
        replacement: &str,
        field_width: usize,
    ) {
        let content = self.get_section_content(section_id);
        let chars: Vec<char> = content.chars().collect();
        let start = start.min(chars.len());
        let end = end.min(chars.len());

        let mut new_content = String::new();
        new_content.extend(chars[..start].iter().copied());
        new_content.push_str(replacement);
        new_content.extend(chars[end..].iter().copied());
        self.set_content_and_cursor(
            section_id,
            new_content,
            start + replacement.chars().count(),
            field_width,
        );
    }

    fn resources_section_id(&self) -> Option<String> {
        self.enabled_sections
            .iter()
            .find(|section_id| Self::section_matches_prefix(section_id, "resources"))
            .cloned()
    }

    fn add_resource_reference(&mut self, full_path: &str) {
        let Some(section_id) = self.resources_section_id() else {
            self.add_section_with_content("resources", full_path.to_string());
            return;
        };

        let content = self.get_section_content(&section_id);
        let updated = if content.is_empty() {
            full_path.to_string()
        } else {
            format!("{content}\n{full_path}")
        };
        self.set_section_content(&section_id, updated);
    }

    /// Replace the `@`-trigger with `@rel_path` in the section text and add the full path
    /// to the resources section (creating one if needed).
    /// Skills are treated as normal file resources — no special content injection.
    pub fn insert_at_completion(
        &mut self,
        section_id: &str,
        rel_path: &str,
        full_path: &str,
        field_width: usize,
    ) {
        let Some(trigger_pos) = self.at_picker.as_ref().map(|picker| picker.trigger_pos) else {
            return;
        };

        // The `@` is at trigger_pos; cursor is currently at trigger_pos + 1
        // (we never insert query chars into the text, only into picker.query).
        self.replace_char_range(
            section_id,
            trigger_pos,
            trigger_pos + 1,
            &format!("@{rel_path}"),
            field_width,
        );
        self.add_resource_reference(full_path);
        // NOTE: focused_section is intentionally NOT restored here.
        // The caller (event handler) owns that responsibility and restores it
        // explicitly after this function returns.
    }

    fn next_file_reference(text: &str, current_pos: usize) -> Option<(usize, &str, usize)> {
        let at_pos = text[current_pos..].find('@')?;
        let absolute_pos = current_pos + at_pos;
        let remaining = &text[absolute_pos..];
        let ref_end = remaining
            .find(|c: char| c.is_whitespace() || c == ',' || c == '!' || c == '?' || c == '│')
            .unwrap_or(remaining.len());
        Some((absolute_pos, &remaining[..ref_end], absolute_pos + ref_end))
    }

    fn is_file_reference(file_ref: &str) -> bool {
        file_ref.len() > 1
            && file_ref[1..]
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-' || c == '.' || c == '/')
    }

    /// Colorize `@word` tokens in rendered section text with a custom accent color.
    pub fn get_file_reference_with_styling(
        &self,
        text: &str,
        accent: Color,
    ) -> Vec<(String, Option<Color>)> {
        let mut result = Vec::new();
        let mut current_pos = 0;

        while let Some((absolute_pos, file_ref, next_pos)) =
            Self::next_file_reference(text, current_pos)
        {
            if absolute_pos > current_pos {
                result.push((text[current_pos..absolute_pos].to_string(), None));
            }

            let color = Self::is_file_reference(file_ref).then_some(accent);
            result.push((file_ref.to_string(), color));
            current_pos = next_pos;
        }

        if current_pos < text.len() {
            result.push((text[current_pos..].to_string(), None));
        }
        result
    }

    /// Count visual (wrapped) lines for a text given a field width
    pub fn visual_line_count(text: &str, field_width: usize) -> usize {
        if field_width == 0 {
            return 1;
        }

        // Keep wrapping math aligned with the rendered paragraph, including tabs
        // and hard line breaks, so box height grows when visible text does.
        let mut lines = 1usize;
        let mut col = 0usize;
        for ch in text.chars() {
            match ch {
                '\n' => {
                    lines += 1;
                    col = 0;
                }
                '\t' => {
                    let tab = 4 - (col % 4);
                    if col + tab > field_width {
                        lines += 1;
                        col = tab;
                    } else {
                        col += tab;
                    }
                }
                _ => {
                    if col + 1 > field_width {
                        lines += 1;
                        col = 1;
                    } else {
                        col += 1;
                    }
                }
            }
        }

        lines.max(1)
    }

    /// Distance from cursor position to the last word boundary (space or newline).
    /// Returns 0 if cursor is at position 0 or no boundary found before cursor.
    pub fn distance_to_last_space(text: &str, cursor_pos: usize) -> usize {
        if cursor_pos == 0 {
            return 0;
        }
        let prefix: String = text.chars().take(cursor_pos).collect();
        // Find the last space or newline
        let last_boundary = prefix.rfind([' ', '\n']);
        match last_boundary {
            Some(pos) => cursor_pos - pos - 1,
            None => cursor_pos, // No boundary found, entire prefix is one word
        }
    }

    /// Check if typing a character should trigger a newline (word-wrap at space boundary).
    /// Returns true if the current word would overflow the available space.
    pub fn should_wrap_at_word_boundary(
        text: &str,
        cursor_pos: usize,
        field_width: usize,
        new_chars: usize,
    ) -> bool {
        if field_width == 0 {
            return false;
        }
        // Calculate current column position (accounting for existing newlines)
        let prefix: String = text.chars().take(cursor_pos).collect();
        let current_col = prefix
            .rfind('\n')
            .map(|pos| cursor_pos - pos - 1)
            .unwrap_or(cursor_pos);

        // Distance from cursor to last space (current word length from last break)
        let word_dist = Self::distance_to_last_space(text, cursor_pos);

        // If adding new_chars would overflow the line
        current_col + word_dist + new_chars > field_width
    }

    /// Visual lines occupied by the first `char_idx` chars of text.
    fn visual_lines_to_cursor(text: &str, char_idx: usize, field_width: usize) -> usize {
        let prefix: String = text.chars().take(char_idx).collect();
        Self::visual_line_count(&prefix, field_width).max(1)
    }

    /// Max visible lines for a section type (instruction=5, others=3)
    pub fn max_visible_lines(section_id: &str) -> usize {
        if Self::section_matches_prefix(section_id, "instruction") {
            5
        } else {
            3
        }
    }

    /// Update scroll for a section so the cursor stays visible.
    pub fn update_section_scroll(&mut self, section_id: &str, field_width: usize) {
        let max_vis = Self::max_visible_lines(section_id);
        let text = self.section_content_for_build(section_id).unwrap_or("");
        let cur = self.cursor(section_id);
        let cursor_visual_line =
            Self::visual_lines_to_cursor(text, cur, field_width).saturating_sub(1);

        let scroll = self
            .section_scrolls
            .entry(section_id.to_string())
            .or_insert(0);
        if cursor_visual_line < *scroll {
            *scroll = cursor_visual_line;
        } else if cursor_visual_line >= *scroll + max_vis {
            *scroll = cursor_visual_line + 1 - max_vis;
        }
    }

    /// Move cursor left one char in the given section.
    pub fn move_cursor_left(&mut self, section_id: &str, field_width: usize) {
        let cur = self.cursor(section_id);
        if cur > 0 {
            self.section_cursors.insert(section_id.to_string(), cur - 1);
            self.update_section_scroll(section_id, field_width);
        }
    }

    /// Move cursor right one char in the given section.
    pub fn move_cursor_right(&mut self, section_id: &str, field_width: usize) {
        let len = self
            .sections
            .get(section_id)
            .map(|s| s.chars().count())
            .unwrap_or(0);
        let cur = self.cursor(section_id);
        if cur < len {
            self.section_cursors.insert(section_id.to_string(), cur + 1);
            self.update_section_scroll(section_id, field_width);
        }
    }

    /// Move cursor up one visual line in the given section.
    pub fn move_cursor_up(&mut self, section_id: &str, field_width: usize) {
        let cur = self.cursor(section_id);
        self.section_cursors
            .insert(section_id.to_string(), cur.saturating_sub(field_width));
        self.update_section_scroll(section_id, field_width);
    }

    /// Move cursor down one visual line in the given section.
    pub fn move_cursor_down(&mut self, section_id: &str, field_width: usize) {
        let len = self
            .sections
            .get(section_id)
            .map(|s| s.chars().count())
            .unwrap_or(0);
        let cur = self.cursor(section_id);
        self.section_cursors
            .insert(section_id.to_string(), (cur + field_width).min(len));
        self.update_section_scroll(section_id, field_width);
    }

    /// Insert a character at cursor position in any section.
    pub fn insert_char_at_cursor(&mut self, section_id: &str, ch: char, field_width: usize) {
        let content = self.get_section_content(section_id);
        let cur = self.cursor(section_id).min(content.chars().count());

        let adjusted_cur = cur;
        let mut modified_content = content.clone();

        if ch != ' ' && ch != '\n' && !content.is_empty()
            && Self::should_wrap_at_word_boundary(&modified_content, adjusted_cur, field_width, 1)
        {
            let prefix: String = modified_content.chars().take(adjusted_cur).collect();
            if let Some(space_pos) = prefix.rfind(' ') {
                let mut chars: Vec<char> = modified_content.chars().collect();
                chars[space_pos] = '\n';
                modified_content = chars.into_iter().collect();
            }
        }

        let mut new_chars: Vec<char> = modified_content.chars().collect();
        new_chars.insert(adjusted_cur, ch);
        let new_content: String = new_chars.into_iter().collect();
        self.set_content_and_cursor(section_id, new_content, adjusted_cur + 1, field_width);
    }

    /// Delete the character before cursor in any section.
    pub fn backspace_at_cursor(&mut self, section_id: &str, field_width: usize) {
        let content = self.get_section_content(section_id);
        let chars: Vec<char> = content.chars().collect();
        let cur = self.cursor(section_id);
        if cur > 0 && cur <= chars.len() {
            let mut new_chars = chars;
            new_chars.remove(cur - 1);
            let new_content: String = new_chars.into_iter().collect();
            self.set_content_and_cursor(section_id, new_content, cur - 1, field_width);
        }
    }

    /// Insert a newline at cursor position in any section.
    pub fn insert_newline_at_cursor(&mut self, section_id: &str, field_width: usize) {
        let (before, after, cursor) = self.split_content_at_cursor(section_id);
        self.set_content_and_cursor(
            section_id,
            format!("{before}\n{after}"),
            cursor + 1,
            field_width,
        );
    }

    /// Insert text at cursor position in any section.
    pub fn insert_text_at_cursor(&mut self, section_id: &str, text: &str, field_width: usize) {
        let (before, after, cursor) = self.split_content_at_cursor(section_id);
        self.set_content_and_cursor(
            section_id,
            format!("{before}{text}{after}"),
            cursor + text.chars().count(),
            field_width,
        );
    }

    fn should_collapse_paste(text: &str) -> bool {
        text.lines().count() > 1 || text.chars().count() > 200
    }

    /// Insert pasted text. If it spans multiple lines, collapse it to a
    /// `[Pasted ~N lines]` placeholder while keeping the real text for `build_prompt`.
    pub fn insert_collapsed_paste_at_cursor(
        &mut self,
        section_id: &str,
        text: &str,
        field_width: usize,
    ) {
        if !Self::should_collapse_paste(text) {
            self.insert_text_at_cursor(section_id, text, field_width);
            return;
        }

        self.expand_collapsed_paste(section_id);
        let (before, after, cursor) = self.split_content_at_cursor(section_id);
        let placeholder = format!("[Pasted ~{} lines]", text.lines().count().max(1));
        self.collapsed_pastes
            .insert(section_id.to_string(), format!("{before}{text}{after}"));
        self.set_content_and_cursor(
            section_id,
            format!("{before}{placeholder}{after}"),
            cursor + placeholder.chars().count(),
            field_width,
        );
    }

    /// Expand a collapsed paste for the given section, restoring real content.
    pub fn expand_collapsed_paste(&mut self, section_id: &str) {
        if let Some(real) = self.collapsed_pastes.remove(section_id) {
            self.set_section_content(section_id, real);
        }
    }

    /// Check if section has a collapsed paste.
    pub fn has_collapsed_paste(&self, section_id: &str) -> bool {
        self.collapsed_pastes.contains_key(section_id)
    }

    /// Check if cursor is positioned inside a collapsed placeholder text.
    /// Returns true if the section has a collapsed paste and cursor is within the placeholder.
    pub fn cursor_in_collapsed_placeholder(&self, section_id: &str) -> bool {
        if !self.has_collapsed_paste(section_id) {
            return false;
        }

        let content = self.get_section_content(section_id);
        let cursor_pos = self.cursor(section_id);

        // Find the collapsed placeholder pattern: "[Pasted ~N lines]"
        if let Some(start) = content.find("[Pasted ~") {
            if let Some(end) = content[start..].find(']') {
                let placeholder_end = start + end + 1;
                return cursor_pos > start && cursor_pos <= placeholder_end;
            }
        }
        false
    }

    /// Delete the entire collapsed paste block and restore cursor position.
    /// Called when backspace is pressed while cursor is inside the placeholder.
    pub fn backspace_collapsed_paste(&mut self, section_id: &str, field_width: usize) {
        if !self.has_collapsed_paste(section_id) {
            return;
        }

        let content = self.get_section_content(section_id);

        // Find and remove the collapsed placeholder
        if let Some(start) = content.find("[Pasted ~") {
            if let Some(end) = content[start..].find(']') {
                let placeholder_end = start + end + 1;
                let mut new_content = String::with_capacity(content.len());
                new_content.push_str(&content[..start]);
                new_content.push_str(&content[placeholder_end..]);

                self.collapsed_pastes.remove(section_id);
                self.set_content_and_cursor(section_id, new_content, start, field_width);
            }
        }
    }
}

fn append_tool_skill(result: &mut String, tool_line: &str) {
    result.push_str("  <skill>\n");
    result.push_str(&format!("    {tool_line}\n"));
    result.push_str("  </skill>\n\n");
}

fn strip_resources_section(prompt: &str) -> String {
    let header = "# [RESOURCES]: Knowledge Base & Data\n<resources>\n";
    let Some(start) = prompt.find(header) else {
        return prompt.to_string();
    };
    let Some(end_rel) = prompt[start..].find("</resources>\n\n") else {
        return prompt.to_string();
    };

    let end = start + end_rel + "</resources>\n\n".len();
    let mut stripped = String::with_capacity(prompt.len().saturating_sub(end - start));
    stripped.push_str(&prompt[..start]);
    stripped.push_str(&prompt[end..]);
    stripped
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn build_prompt_resolves_project_context_and_file_resources() {
        let temp = tempdir().unwrap();
        let db_path = temp.path().join("canopy.db");
        let db = Database::new(&db_path).unwrap();

        let project_dir = temp.path().join("sample-project");
        std::fs::create_dir(&project_dir).unwrap();
        let project = db.register_project_path(&project_dir).unwrap();

        let resource = project_dir.join("guide.txt");
        std::fs::write(&resource, "hello from resource").unwrap();

        let mut dialog = SimplePromptDialog::new();
        dialog.set_section_content("instruction", "do the thing".to_string());
        dialog.add_section_with_content("project_context", project.path.clone());
        dialog.add_section_with_content("resources", resource.display().to_string());

        let prompt = dialog
            .build_prompt_with_resolved_resources(&db, &project_dir)
            .unwrap();

        assert!(prompt.contains("# [PROJECT CONTEXT]: Registered Project Metadata"));
        assert!(prompt.contains(&project.hash));
        assert!(prompt.contains("kind: file"));
        assert!(prompt.contains("guide.txt"));
    }

    #[test]
    fn build_prompt_resolves_project_directory_in_resources() {
        let temp = tempdir().unwrap();
        let db_path = temp.path().join("canopy.db");
        let db = Database::new(&db_path).unwrap();

        let project_dir = temp.path().join("dir-project");
        std::fs::create_dir(&project_dir).unwrap();
        let project = db.register_project_path(&project_dir).unwrap();

        let mut dialog = SimplePromptDialog::new();
        dialog.set_section_content("instruction", "summarize".to_string());
        dialog.add_section_with_content("resources", project.path.clone());

        let prompt = dialog
            .build_prompt_with_resolved_resources(&db, &project_dir)
            .unwrap();

        assert!(prompt.contains("workdir_hash:"));
        assert!(prompt.contains(&project.hash));
    }

    #[test]
    fn build_prompt_auto_injects_project_context_for_resource_descendant() {
        let temp = tempdir().unwrap();
        let db_path = temp.path().join("canopy.db");
        let db = Database::new(&db_path).unwrap();

        let project_dir = temp.path().join("linked-project");
        std::fs::create_dir(&project_dir).unwrap();
        let project = db.register_project_path(&project_dir).unwrap();

        let nested_dir = project_dir.join("src").join("module");
        std::fs::create_dir_all(&nested_dir).unwrap();
        let file_path = nested_dir.join("lib.rs");
        std::fs::write(&file_path, "pub fn demo() {}").unwrap();

        let mut dialog = SimplePromptDialog::new();
        dialog.set_section_content("instruction_1", "summarize".to_string());
        dialog.add_section_with_content("resources", file_path.display().to_string());

        let prompt = dialog
            .build_prompt_with_resolved_resources(&db, temp.path())
            .unwrap();

        assert!(prompt.contains("# [PROJECT CONTEXT]: Registered Project Metadata"));
        assert!(prompt.contains(&project.hash));
        assert!(prompt.contains("kind: file"));
        assert!(prompt.contains("lib.rs"));
    }

    #[test]
    fn migrate_legacy_sections_replaces_memory_with_project_context() {
        let mut dialog = SimplePromptDialog::new();
        dialog.add_section_with_content("memory_context", "legacy".to_string());
        dialog.add_section_with_content("examples", "old".to_string());

        dialog.migrate_legacy_sections(Some("/tmp/project"));

        assert!(dialog
            .enabled_sections
            .iter()
            .all(|section_id| !section_id.starts_with("memory_context_")
                && !section_id.starts_with("examples_")));
        assert!(dialog
            .enabled_sections
            .iter()
            .any(|section_id| section_id.starts_with("project_context_")));
    }

    #[test]
    fn distance_to_last_space_basic() {
        assert_eq!(
            SimplePromptDialog::distance_to_last_space("hello world", 11),
            5
        );
        assert_eq!(
            SimplePromptDialog::distance_to_last_space("hello world", 6),
            0
        );
        assert_eq!(SimplePromptDialog::distance_to_last_space("hello", 5), 5);
        assert_eq!(SimplePromptDialog::distance_to_last_space("", 0), 0);
    }

    #[test]
    fn distance_to_last_space_after_space() {
        assert_eq!(SimplePromptDialog::distance_to_last_space("hello w", 7), 1);
        assert_eq!(SimplePromptDialog::distance_to_last_space("a b c d", 7), 1);
    }

    #[test]
    fn should_wrap_at_word_boundary_basic() {
        // "hello " is 6 chars, adding "world" (5 chars) = 11 chars
        // With field_width=10, should wrap
        assert!(SimplePromptDialog::should_wrap_at_word_boundary(
            "hello ", 6, 10, 5
        ));
        // With field_width=20, should not wrap
        assert!(!SimplePromptDialog::should_wrap_at_word_boundary(
            "hello ", 6, 20, 5
        ));
    }

    #[test]
    fn should_wrap_at_word_boundary_with_newline() {
        // After a newline, column resets to 0
        assert!(!SimplePromptDialog::should_wrap_at_word_boundary(
            "hello\n", 6, 10, 5
        ));
        assert!(SimplePromptDialog::should_wrap_at_word_boundary(
            "hello\nworld ",
            12,
            10,
            5
        ));
    }

    #[test]
    fn should_wrap_at_word_boundary_at_typing() {
        // Typing one char at a time: "hello " + "w" = cursor at 7, word_dist = 1
        // current_col = 7, word_dist = 1, new_chars = 1 → 7 + 1 + 1 = 9 < 10, no wrap
        assert!(!SimplePromptDialog::should_wrap_at_word_boundary(
            "hello w", 7, 10, 1
        ));
        // "hello worl" + "d" = cursor at 11, word_dist = 5
        // current_col = 11, word_dist = 5, new_chars = 1 → 11 + 5 + 1 = 17 > 10, wrap
        assert!(SimplePromptDialog::should_wrap_at_word_boundary(
            "hello worl",
            10,
            10,
            1
        ));
    }
}

/// Snapshot of `SimplePromptDialog` state used to persist the prompt builder
/// per agent/session across openings within the same canopy TUI session.
#[derive(Clone)]
pub struct PromptBuilderSession {
    pub sections: HashMap<String, String>,
    pub enabled_sections: Vec<String>,
    pub focused_section: usize,
    pub section_counters: HashMap<String, usize>,
    pub section_cursors: HashMap<String, usize>,
    pub section_scrolls: HashMap<String, usize>,
    pub collapsed_pastes: HashMap<String, String>,
    pub locked_sections: HashSet<String>,
}

impl PromptBuilderSession {
    pub fn from_dialog(dialog: &SimplePromptDialog) -> Self {
        Self {
            sections: dialog.sections.clone(),
            enabled_sections: dialog.enabled_sections.clone(),
            focused_section: dialog.focused_section,
            section_counters: dialog.section_counters.clone(),
            section_cursors: dialog.section_cursors.clone(),
            section_scrolls: dialog.section_scrolls.clone(),
            collapsed_pastes: dialog.collapsed_pastes.clone(),
            locked_sections: dialog.locked_sections.clone(),
        }
    }

    pub fn restore_into(&self, dialog: &mut SimplePromptDialog) {
        dialog.sections = self.sections.clone();
        dialog.enabled_sections = self.enabled_sections.clone();
        dialog.focused_section = self.focused_section;
        dialog.section_counters = self.section_counters.clone();
        dialog.section_cursors = self.section_cursors.clone();
        dialog.section_scrolls = self.section_scrolls.clone();
        dialog.collapsed_pastes = self.collapsed_pastes.clone();
        dialog.locked_sections = self.locked_sections.clone();
        // Reset transient UI state (not persisted across openings)
        dialog.picker_mode = SectionPickerMode::None;
        dialog.at_picker = None;
        dialog.system_content = None; // re-evaluated on each open
    }
}

// ── Prompt builder helpers ────────────────────────────────────────
// Skill discovery helpers

fn add_skills_from_dir(
    dir: &std::path::Path,
    prefix: &str,
    out: &mut Vec<(String, String, String)>,
) {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in rd.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let Some(raw_name) = path
            .file_name()
            .and_then(|n| n.to_str())
            .map(str::to_string)
        else {
            continue;
        };
        if crate::skills_module::find_skill_instructions(&path).is_none() {
            continue;
        }
        out.push((format!("skill:{raw_name}"), raw_name, prefix.to_string()));
    }
}

// XML formatting helpers

fn push_xml_item(result: &mut String, tag: &str, content: &str) {
    result.push_str(&format!("  <{tag}>\n"));
    for line in content.lines() {
        result.push_str(&format!("    {line}\n"));
    }
    result.push_str(&format!("  </{tag}>\n\n"));
}

fn format_indexed_xml_section(
    header: &str,
    outer_tag: &str,
    item_tag: &str,
    items: &[String],
) -> Option<String> {
    if items.is_empty() {
        return None;
    }

    let mut result = String::new();
    result.push_str(header);
    result.push_str(&format!("<{outer_tag}>\n"));
    for item in items.iter() {
        push_xml_item(&mut result, item_tag, item);
    }
    result.push_str(&format!("</{outer_tag}>\n\n"));
    Some(result)
}

/// Build a wrapped XML section (header + outer tag + items) from matching section IDs.
fn build_xml_block<'a>(
    result: &mut String,
    sections: &'a [String],
    matches: impl Fn(&str) -> bool,
    content_for: impl Fn(&'a str) -> Option<&'a str>,
    header: &str,
    outer_tag: &str,
    item_tag: &str,
) {
    let mut count = 0;
    for id in sections {
        if !matches(id) {
            continue;
        }
        let Some(content) = content_for(id) else {
            continue;
        };
        let trimmed = content.trim();
        if trimmed.is_empty() {
            continue;
        }
        if count == 0 {
            result.push_str(header);
            result.push_str(&format!("<{outer_tag}>\n"));
        }
        count += 1;
        push_xml_item(result, item_tag, trimmed);
    }
    if count > 0 {
        result.push_str(&format!("</{outer_tag}>\n\n"));
    }
}
