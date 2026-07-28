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
    migrate_loop_design_rename(&global);
    let client = build_github_client()?;

    let registry = fetch_skills_registry(&client);

    let Some(entries) = fetch_essential_pack_entries(&client)? else {
        return Ok(0);
    };

    sync_skill_dirs(&client, &global, &entries, &registry, force)
}

/// Explicit rename migration (S3): the Essential Pack catalog renamed
/// `loop-design` to `canopy-loop-design` (canopy-family naming, gated by
/// `requires = "canopy"`). A machine that installed the pack before the
/// rename has `~/.agents/skills/loop-design/` on disk; left alone, that
/// directory would become a permanent stale copy — its name no longer
/// appears in the catalog, so the per-file sync loop below would never
/// touch it again. Moving it under the new name makes it a live entry
/// again, so the normal content-hash sync (`sync_policy::sync_write`) takes
/// over from here for every file inside it. This migration only ever moves
/// the directory, never its content, so it can't violate B4's
/// never-clobber-newer-local-content rule.
///
/// If `canopy-loop-design` already exists too, the rename is skipped rather
/// than guessing which copy should win — the legacy directory is left in
/// place with a log line for manual reconciliation.
fn migrate_loop_design_rename(global: &Path) {
    let old = global.join("loop-design");
    let new = global.join("canopy-loop-design");
    if !old.exists() {
        return;
    }
    if new.exists() {
        tracing::warn!(
            "Skills sync: both {} and {} exist; leaving the legacy directory in place \
             for manual reconciliation instead of guessing which one should win.",
            old.display(),
            new.display()
        );
        return;
    }
    match std::fs::rename(&old, &new) {
        Ok(()) => tracing::info!(
            "Skills sync: migrated renamed skill directory {} -> {}",
            old.display(),
            new.display()
        ),
        Err(e) => tracing::warn!(
            "Skills sync: failed to migrate {} -> {}: {e}",
            old.display(),
            new.display()
        ),
    }
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
        let path_value = std::env::var("PATH").unwrap_or_default();
        self.should_install_with_path(skill_name, &path_value)
    }

    /// PATH-injectable core of [`Self::should_install`] so the `requires`
    /// resolution logic (e.g. the canopy-family skills' `requires =
    /// "canopy"`) can be unit tested against a synthetic PATH instead of
    /// racing the real process environment.
    fn should_install_with_path(&self, skill_name: &str, path_value: &str) -> bool {
        match self.requires.get(skill_name) {
            Some(binary) => {
                crate::domain::cli_strategy::resolve_binary_in(binary, path_value).is_ok()
            }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn migrate_loop_design_rename_moves_legacy_directory_to_new_name() {
        let dir = tempfile::tempdir().unwrap();
        let old = dir.path().join("loop-design");
        std::fs::create_dir_all(&old).unwrap();
        std::fs::write(old.join("SKILL.md"), b"loop-design content").unwrap();

        migrate_loop_design_rename(dir.path());

        assert!(!old.exists());
        let new = dir.path().join("canopy-loop-design");
        assert!(new.exists());
        assert_eq!(
            std::fs::read(new.join("SKILL.md")).unwrap(),
            b"loop-design content"
        );
    }

    #[test]
    fn migrate_loop_design_rename_is_noop_when_legacy_directory_absent() {
        let dir = tempfile::tempdir().unwrap();
        // Neither directory exists — must not panic or create anything.
        migrate_loop_design_rename(dir.path());
        assert!(!dir.path().join("loop-design").exists());
        assert!(!dir.path().join("canopy-loop-design").exists());
    }

    #[test]
    fn migrate_loop_design_rename_leaves_both_when_new_name_already_exists() {
        let dir = tempfile::tempdir().unwrap();
        let old = dir.path().join("loop-design");
        let new = dir.path().join("canopy-loop-design");
        std::fs::create_dir_all(&old).unwrap();
        std::fs::write(old.join("SKILL.md"), b"legacy content").unwrap();
        std::fs::create_dir_all(&new).unwrap();
        std::fs::write(new.join("SKILL.md"), b"already-migrated content").unwrap();

        migrate_loop_design_rename(dir.path());

        // Neither directory is touched — B4 never-clobber, no guessing which wins.
        assert!(old.exists());
        assert_eq!(
            std::fs::read(old.join("SKILL.md")).unwrap(),
            b"legacy content"
        );
        assert_eq!(
            std::fs::read(new.join("SKILL.md")).unwrap(),
            b"already-migrated content"
        );
    }

    #[test]
    fn parse_skills_toml_reads_canopy_family_requires_entries() {
        // Mirrors the redesigned skills.toml (S3): the new canopy-family
        // skills are all gated on the `canopy` binary being in PATH.
        let toml = r#"
[skills.canopy-intelligence]
requires = "canopy"

[skills.canopy-sync]
requires = "canopy"

[skills.canopy-loop-design]
requires = "canopy"

[skills.canopy-capabilities]
requires = "canopy"

[skills.architect-mindset]
"#;
        let registry = parse_skills_toml(toml);

        for name in [
            "canopy-intelligence",
            "canopy-sync",
            "canopy-loop-design",
            "canopy-capabilities",
        ] {
            assert_eq!(
                registry.requires.get(name).map(String::as_str),
                Some("canopy"),
                "'{name}' must require the 'canopy' binary"
            );
        }
        // A skill with no `requires` key is simply absent from the map.
        assert!(!registry.requires.contains_key("architect-mindset"));
    }

    /// Build a synthetic single-directory PATH containing an executable
    /// named `name`, so `should_install_with_path` can be tested without
    /// touching (and racing) the real process `PATH`.
    fn path_with_fake_binary(name: &str) -> (tempfile::TempDir, String) {
        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join(name);
        std::fs::write(&bin, b"#!/bin/sh\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let path_value = dir.path().to_string_lossy().to_string();
        (dir, path_value)
    }

    #[test]
    fn should_install_resolves_requires_canopy_when_canopy_binary_is_in_path() {
        // S3: canopy-family skills (requires = "canopy") must install
        // whenever canopy itself is running the setup — trivially true
        // since the running binary's own directory is on PATH, but this
        // proves the resolution logic that guarantees it actually works.
        let (_dir, path_value) = path_with_fake_binary("canopy");
        let mut registry = SkillsRegistry::default();
        registry
            .requires
            .insert("canopy-loop-design".to_string(), "canopy".to_string());

        assert!(registry.should_install_with_path("canopy-loop-design", &path_value));
    }

    #[test]
    fn should_install_returns_false_when_required_binary_missing_from_path() {
        let empty_path = "";
        let mut registry = SkillsRegistry::default();
        registry
            .requires
            .insert("canopy-loop-design".to_string(), "canopy".to_string());

        assert!(!registry.should_install_with_path("canopy-loop-design", empty_path));
    }

    #[test]
    fn should_install_is_true_without_a_requires_entry() {
        let registry = SkillsRegistry::default();
        assert!(registry.should_install_with_path("architect-mindset", ""));
    }
}
