//! Essential Pack download — fetches skills from GitHub into `~/.agents/skills/`.
//!
//! Skills with `requires` in skills.toml are only installed if the binary is in PATH.

use anyhow::{Context, Result};
use std::collections::HashMap;
use std::path::Path;

use super::ensure_global_skills_dir;
use super::sync_policy;

const ESSENTIAL_PACK_REPO: &str = "UniverLab/skills";
const ESSENTIAL_PACK_API: &str = "https://api.github.com/repos/UniverLab/skills/contents";
const SKILLS_TOML_URL: &str = "https://raw.githubusercontent.com/UniverLab/skills/main/skills.toml";

/// Download the Essential Pack from GitHub into `~/.agents/skills/`.
///
/// Existing files whose content diverges from the incoming sync source are
/// left untouched (a WARN is logged and a `.sync-new` sidecar is written)
/// unless `force` is set. See `sync_policy` for the decision logic.
pub fn download_essential_pack(force: bool) -> Result<usize> {
    let global = ensure_global_skills_dir()?;
    let client = build_github_client()?;

    let registry = fetch_skills_registry(&client);

    let Some(entries) = fetch_essential_pack_entries(&client)? else {
        return Ok(0);
    };

    sync_skill_dirs(&client, &global, &entries, &registry, force)
}

fn build_github_client() -> Result<reqwest::blocking::Client> {
    reqwest::blocking::Client::builder()
        .user_agent("canopy")
        .build()
        .map_err(Into::into)
}

fn fetch_skills_registry(client: &reqwest::blocking::Client) -> SkillsRegistry {
    let Ok(response) = client.get(SKILLS_TOML_URL).send() else {
        tracing::debug!("Could not fetch skills.toml, installing all skills");
        return SkillsRegistry::default();
    };

    if !response.status().is_success() {
        tracing::debug!(
            "skills.toml not found ({}), installing all skills",
            response.status()
        );
        return SkillsRegistry::default();
    }

    let Ok(content) = response.text() else {
        return SkillsRegistry::default();
    };

    parse_skills_toml(&content)
}

fn parse_skills_toml(content: &str) -> SkillsRegistry {
    let Ok(parsed) = content.parse::<toml::Table>() else {
        tracing::warn!("Failed to parse skills.toml");
        return SkillsRegistry::default();
    };

    let mut registry = SkillsRegistry::default();

    if let Some(skills) = parsed.get("skills").and_then(|v| v.as_table()) {
        for (name, value) in skills {
            if let Some(skill_config) = value.as_table() {
                if let Some(requires) = skill_config.get("requires").and_then(|v| v.as_str()) {
                    registry.requires.insert(name.clone(), requires.to_string());
                }
            }
        }
    }

    registry
}

#[derive(Default)]
struct SkillsRegistry {
    requires: HashMap<String, String>,
}

impl SkillsRegistry {
    fn should_install(&self, skill_name: &str) -> bool {
        match self.requires.get(skill_name) {
            Some(binary) => which::which(binary).is_ok(),
            None => true,
        }
    }
}

fn fetch_essential_pack_entries(
    client: &reqwest::blocking::Client,
) -> Result<Option<Vec<GhEntry>>> {
    let response = client
        .get(ESSENTIAL_PACK_API)
        .send()
        .context("Failed to connect to GitHub API")?;

    if !response.status().is_success() {
        tracing::warn!(
            "GitHub API returned {} for {}; skipping essential skills download.",
            response.status(),
            ESSENTIAL_PACK_REPO
        );
        return Ok(None);
    }

    let entries = response
        .json()
        .context("Failed to parse GitHub API response")?;
    Ok(Some(entries))
}

/// Sync every skill directory from the Essential Pack into `global`.
///
/// A skill directory that already exists locally is still visited — its
/// files are synced individually under the content-hash overwrite policy —
/// rather than being skipped wholesale, so legitimate upstream updates still
/// land as long as they don't clobber local divergence.
fn sync_skill_dirs(
    client: &reqwest::blocking::Client,
    global: &Path,
    entries: &[GhEntry],
    registry: &SkillsRegistry,
    force: bool,
) -> Result<usize> {
    let mut synced = 0usize;

    for entry in entries.iter().filter(|entry| entry.entry_type == "dir") {
        if !registry.should_install(&entry.name) {
            tracing::debug!(
                "Skipping skill '{}': binary '{}' not found in PATH",
                entry.name,
                registry.requires.get(&entry.name).unwrap_or(&String::new())
            );
            continue;
        }

        let skill_dir = global.join(&entry.name);
        if sync_skill_dir(client, &entry.name, &skill_dir, force)? {
            synced += 1;
        }
    }

    Ok(synced)
}

fn sync_skill_dir(
    client: &reqwest::blocking::Client,
    skill_name: &str,
    skill_dir: &Path,
    force: bool,
) -> Result<bool> {
    let Some(dir_entries) = fetch_skill_dir_entries(client, skill_name)? else {
        return Ok(false);
    };
    if !has_skill_instructions_entry(&dir_entries) {
        return Ok(false);
    }

    std::fs::create_dir_all(skill_dir)?;
    Ok(write_skill_files(client, skill_dir, &dir_entries, force))
}

fn fetch_skill_dir_entries(
    client: &reqwest::blocking::Client,
    skill_name: &str,
) -> Result<Option<Vec<GhEntry>>> {
    let dir_url = format!("{ESSENTIAL_PACK_API}/{skill_name}");
    let Ok(response) = client.get(&dir_url).send() else {
        return Ok(None);
    };
    if !response.status().is_success() {
        return Ok(None);
    }

    let Ok(entries) = response.json() else {
        return Ok(None);
    };
    Ok(Some(entries))
}

fn has_skill_instructions_entry(entries: &[GhEntry]) -> bool {
    entries
        .iter()
        .any(|entry| matches!(entry.name.as_str(), "SKILL.md" | "INSTRUCTIONS.md"))
}

/// Writes every file entry, applying the content-hash overwrite policy per
/// file. Returns `true` if at least one file was actually written.
fn write_skill_files(
    client: &reqwest::blocking::Client,
    skill_dir: &Path,
    entries: &[GhEntry],
    force: bool,
) -> bool {
    let mut wrote_any = false;
    for file in entries.iter().filter(|entry| entry.entry_type == "file") {
        if write_skill_file(client, skill_dir, file, force) {
            wrote_any = true;
        }
    }
    wrote_any
}

fn write_skill_file(
    client: &reqwest::blocking::Client,
    skill_dir: &Path,
    file: &GhEntry,
    force: bool,
) -> bool {
    let Some(raw_url) = file.download_url.as_deref() else {
        return false;
    };

    let Ok(response) = client.get(raw_url).send() else {
        return false;
    };
    if !response.status().is_success() {
        return false;
    }

    let Ok(content) = response.bytes() else {
        return false;
    };

    let dest = skill_dir.join(&file.name);
    match sync_policy::sync_write(&dest, raw_url, &content, force) {
        Ok(wrote) => wrote,
        Err(e) => {
            tracing::warn!("Skills sync: failed to write {}: {e}", dest.display());
            false
        }
    }
}

#[derive(serde::Deserialize)]
struct GhEntry {
    name: String,
    #[serde(rename = "type")]
    entry_type: String,
    download_url: Option<String>,
}
