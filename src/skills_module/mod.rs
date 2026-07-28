//! Global skill standard — `~/.agents/skills/` management.
//!
//! Skills live in a single master directory (`~/.agents/skills/`).
//! Canopy creates symlinks from each platform's own skills folder to the
//! master directory so every agent always sees the same set of skills.
//!
//! Layout:
//! ```text
//! ~/.agents/skills/
//!   code-review/
//!     SKILL.md ← instructions injected by the @ picker
//!   rust-idiomatic-patterns/
//!     SKILL.md
//! ~/.kiro/skills/code-review → ~/.agents/skills/code-review (symlink)
//! ```

mod download;
mod sync_policy;
mod wizard;

use anyhow::{Context, Result};
use std::path::{Path, PathBuf};

pub use download::download_essential_pack;
#[allow(unused_imports)]
pub use wizard::run_skills_wizard;

/// Well-known skills master directory path.
pub fn global_skills_dir() -> Option<PathBuf> {
    dirs::home_dir().map(|h| global_skills_dir_for(&h))
}

/// Ensure the global skills directory exists.
pub fn ensure_global_skills_dir() -> Result<PathBuf> {
    let dir = global_skills_dir().context("No home directory")?;
    std::fs::create_dir_all(&dir)?;
    Ok(dir)
}

/// Create (or repair) symlinks from each platform's skills directory to the
/// global skills master.
///
/// A symlink is created for every immediate child directory inside
/// `~/.agents/skills/`. Each platform that exposes a `skills_dir` in the
/// registry gets a per-skill symlink `<platform_skills_dir>/<skill_name>`.
pub fn create_platform_symlinks(
    home: &Path,
    platforms: &[&crate::setup_module::Platform],
) -> Result<Vec<String>> {
    let global = global_skills_dir_for(home);
    if !global.exists() {
        return Ok(Vec::new());
    }

    let skill_entries = list_skill_dirs(&global);
    let mut created = Vec::new();

    for (platform, platform_skills) in platform_skill_dirs(home, platforms) {
        create_platform_symlinks_for_platform(
            platform,
            &platform_skills,
            &global,
            &skill_entries,
            &mut created,
        );
    }

    Ok(created)
}

fn create_platform_symlinks_for_platform(
    platform: &crate::setup_module::Platform,
    platform_skills: &Path,
    global: &Path,
    skill_entries: &[String],
    created: &mut Vec<String>,
) {
    if !ensure_platform_skills_dir(platform, platform_skills) {
        return;
    }

    for skill_name in skill_entries {
        if let Some(created_link) =
            create_platform_skill_link(&platform.name, platform_skills, global, skill_name)
        {
            created.push(created_link);
        }
    }
}

fn create_platform_skill_link(
    platform_name: &str,
    platform_skills: &Path,
    global: &Path,
    skill_name: &str,
) -> Option<String> {
    let link = platform_skills.join(skill_name);
    if link.exists() || link.is_symlink() {
        return None;
    }

    let target = global.join(skill_name);
    if let Err(error) = install_skill_link(&target, &link) {
        tracing::warn!("Symlink {}: {}", link.display(), error);
        return None;
    }

    Some(format!("{platform_name}/{skill_name}"))
}

/// Validate symlink integrity: return broken symlink paths.
/// Used by the `canopy skills` wizard (future subcommand).
#[allow(dead_code)]
pub fn find_broken_symlinks(
    home: &Path,
    platforms: &[&crate::setup_module::Platform],
) -> Vec<PathBuf> {
    platform_skill_dirs(home, platforms)
        .flat_map(|(_, platform_skills)| broken_symlinks_in_dir(&platform_skills))
        .collect()
}

fn broken_symlinks_in_dir(dir: &Path) -> Vec<PathBuf> {
    if !dir.exists() {
        return Vec::new();
    }

    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };

    entries
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.is_symlink() && !path.exists())
        .collect()
}

/// List immediate subdirectory names inside `dir` that look like skill folders
/// (contain a `SKILL.md` or `INSTRUCTIONS.md`).
pub fn list_skill_dirs(dir: &Path) -> Vec<String> {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut names: Vec<String> = rd
        .flatten()
        .filter(|e| e.file_type().map(|t| t.is_dir()).unwrap_or(false))
        .filter(|e| contains_skill_instructions(&e.path()))
        .filter_map(|e| e.file_name().to_str().map(|s| s.to_string()))
        .collect();
    names.sort();
    names
}

/// Locate the instructions file for a skill directory.
/// Returns `Some(path)` for `SKILL.md`, then `INSTRUCTIONS.md`, otherwise `None`.
pub fn find_skill_instructions(skill_dir: &Path) -> Option<PathBuf> {
    ["SKILL.md", "INSTRUCTIONS.md"]
        .into_iter()
        .map(|name| skill_dir.join(name))
        .find(|path| path.exists())
}

// ── Utilities (shared with submodules) ────────────────────────────────────

pub(super) fn global_skills_dir_for(home: &Path) -> PathBuf {
    home.join(".agents").join("skills")
}

fn contains_skill_instructions(dir: &Path) -> bool {
    find_skill_instructions(dir).is_some()
}

pub(super) fn platform_skill_dirs<'a>(
    home: &'a Path,
    platforms: &'a [&'a crate::setup_module::Platform],
) -> impl Iterator<Item = (&'a crate::setup_module::Platform, PathBuf)> + 'a {
    platforms.iter().copied().filter_map(move |platform| {
        platform_skill_dir(home, platform).map(|skills_dir| (platform, skills_dir))
    })
}

fn platform_skill_dir(home: &Path, platform: &crate::setup_module::Platform) -> Option<PathBuf> {
    let skills_dir = platform.skills_dir.as_deref()?;
    Some(home.join(skills_dir))
}

fn ensure_platform_skills_dir(platform: &crate::setup_module::Platform, path: &Path) -> bool {
    if let Err(error) = std::fs::create_dir_all(path) {
        tracing::warn!(
            "Could not create skills dir for {}: {}",
            platform.name,
            error
        );
        return false;
    }

    true
}

#[cfg(unix)]
fn install_skill_link(target: &Path, link: &Path) -> Result<()> {
    std::os::unix::fs::symlink(target, link)?;
    Ok(())
}

#[cfg(not(unix))]
fn install_skill_link(target: &Path, link: &Path) -> Result<()> {
    copy_dir_recursive(target, link)
}

/// Recursively copy a directory (used on non-Unix systems as symlink fallback).
#[cfg(not(unix))]
fn copy_dir_recursive(src: &Path, dst: &Path) -> Result<()> {
    std::fs::create_dir_all(dst)?;
    for entry in std::fs::read_dir(src)?.flatten() {
        let dest = dst.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            copy_dir_recursive(&entry.path(), &dest)?;
        } else {
            std::fs::copy(&entry.path(), &dest)?;
        }
    }
    Ok(())
}
