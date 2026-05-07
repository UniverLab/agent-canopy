//! Essential Pack download — fetches skills from GitHub into `~/.agents/skills/`.

use anyhow::{Context, Result};
use std::path::Path;

use super::ensure_global_skills_dir;

const ESSENTIAL_PACK_REPO: &str = "UniverLab/skills";
const ESSENTIAL_PACK_API: &str = "https://api.github.com/repos/UniverLab/skills/contents";

pub fn download_essential_pack() -> Result<usize> {
    let global = ensure_global_skills_dir()?;
    let client = build_github_client()?;
    let Some(entries) = fetch_essential_pack_entries(&client)? else {
        return Ok(0);
    };

    download_missing_skill_dirs(&client, &global, &entries)
}

fn build_github_client() -> Result<reqwest::blocking::Client> {
    reqwest::blocking::Client::builder()
        .user_agent("canopy")
        .build()
        .map_err(Into::into)
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

fn download_missing_skill_dirs(
    client: &reqwest::blocking::Client,
    global: &Path,
    entries: &[GhEntry],
) -> Result<usize> {
    let mut downloaded = 0usize;

    for entry in entries.iter().filter(|entry| entry.entry_type == "dir") {
        let skill_dir = global.join(&entry.name);
        if skill_dir.exists() {
            continue;
        }

        if download_skill_dir(client, &entry.name, &skill_dir)? {
            downloaded += 1;
        }
    }

    Ok(downloaded)
}

fn download_skill_dir(
    client: &reqwest::blocking::Client,
    skill_name: &str,
    skill_dir: &Path,
) -> Result<bool> {
    let Some(dir_entries) = fetch_skill_dir_entries(client, skill_name)? else {
        return Ok(false);
    };
    if !has_skill_instructions_entry(&dir_entries) {
        return Ok(false);
    }

    std::fs::create_dir_all(skill_dir)?;
    write_skill_files(client, skill_dir, &dir_entries);
    Ok(true)
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

fn write_skill_files(client: &reqwest::blocking::Client, skill_dir: &Path, entries: &[GhEntry]) {
    for file in entries.iter().filter(|entry| entry.entry_type == "file") {
        write_skill_file(client, skill_dir, file);
    }
}

fn write_skill_file(client: &reqwest::blocking::Client, skill_dir: &Path, file: &GhEntry) {
    let Some(raw_url) = file.download_url.as_deref() else {
        return;
    };

    let Ok(response) = client.get(raw_url).send() else {
        return;
    };
    if !response.status().is_success() {
        return;
    }

    let Ok(content) = response.bytes() else {
        return;
    };
    let _ = std::fs::write(skill_dir.join(&file.name), &content);
}

#[derive(serde::Deserialize)]
struct GhEntry {
    name: String,
    #[serde(rename = "type")]
    entry_type: String,
    download_url: Option<String>,
}
