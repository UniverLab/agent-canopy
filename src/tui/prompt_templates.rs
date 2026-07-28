//! Prompt templates — internal structured prompt templates.

use anyhow::{anyhow, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// Individual section in a prompt template
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TemplateSection {
    pub name: String,
    pub label: String,
    pub placeholder: String,
    pub required: bool,
}

/// Complete prompt template definition
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PromptTemplate {
    pub name: String,
    pub description: String,
    pub sections: Vec<TemplateSection>,
    pub format: String,
}

/// Collection of all available prompt templates
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PromptTemplates {
    pub version: String,
    pub templates: Vec<PromptTemplate>,
}

impl PromptTemplates {
    /// Load templates - now using internal templates only
    pub fn load_from_registry() -> Result<Self> {
        Ok(Self::internal_templates())
    }

    /// Internal templates - no registry dependency
    pub fn internal_templates() -> Self {
        Self {
            version: "1.0".to_string(),
            templates: vec![PromptTemplate {
                name: "simple".to_string(),
                description: "Simple prompt template with optional sections".to_string(),
                sections: vec![
                    TemplateSection {
                        name: "instruction".to_string(),
                        label: "Instruction".to_string(),
                        placeholder: "What do you want to accomplish?".to_string(),
                        required: true,
                    },
                    TemplateSection {
                        name: "context".to_string(),
                        label: "Context".to_string(),
                        placeholder: "Relevant background information".to_string(),
                        required: false,
                    },
                    TemplateSection {
                        name: "resources".to_string(),
                        label: "Resources".to_string(),
                        placeholder: "Available tools or resources".to_string(),
                        required: false,
                    },
                ],
                format: "{{instruction}}".to_string(),
            }],
        }
    }

    /// Get template by name
    #[allow(dead_code)]
    pub fn get_template(&self, name: &str) -> Option<&PromptTemplate> {
        self.templates.iter().find(|t| t.name == name)
    }

    /// Get the default template
    #[allow(dead_code)]
    pub fn get_default(&self) -> &PromptTemplate {
        self.templates.first().unwrap_or_else(|| {
            &self.templates[0] // This should never panic due to internal_templates()
        })
    }

    /// Build a prompt from filled sections (only includes non-empty sections)
    #[allow(dead_code)]
    pub fn build_prompt(
        &self,
        template_name: &str,
        sections: &HashMap<String, String>,
    ) -> Result<String> {
        let template = self
            .get_template(template_name)
            .ok_or_else(|| anyhow!("Template {} not found", template_name))?;

        let mut result = String::new();

        // Start with instruction (always required)
        if let Some(instruction) = sections.get("instruction") {
            if !instruction.is_empty() {
                result.push_str(instruction);
            }
        }

        // Add optional sections if they have content
        for section in &template.sections {
            if section.name == "instruction" {
                continue; // Already handled
            }
            if let Some(content) = sections.get(&section.name) {
                if !content.is_empty() {
                    result.push_str("\n\n");
                    result.push_str(&section.label);
                    result.push_str(": ");
                    result.push_str(content);
                }
            }
        }

        Ok(result)
    }
}
