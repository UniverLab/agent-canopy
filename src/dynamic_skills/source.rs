//! Git-backed skill sources.
//!
//! A source is a git repo where each top-level directory containing a
//! `SKILL.md`/`INSTRUCTIONS.md` is a skill. `GitSource` never keeps a
//! persistent clone around: every call shells out to a fresh temp clone
//! (shallow, and sparse when fetching a single skill) and cleans up after
//! itself.

use anyhow::{bail, Context, Result};
use std::path::Path;
use std::process::Command;

/// One skill directory found in a source's catalog, with its parsed
/// one-line description.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct CatalogEntry {
    pub name: String,
    pub description: String,
}

/// A configured git skill source: a URL (https, ssh, or local path) plus an
/// optional branch/tag to track.
#[derive(Debug, Clone)]
pub struct GitSource {
    pub url: String,
    pub git_ref: Option<String>,
}

impl GitSource {
    pub fn new(url: impl Into<String>, git_ref: Option<String>) -> Self {
        Self {
            url: url.into(),
            git_ref,
        }
    }

    /// Cheap network call: resolve the current commit hash of the
    /// configured ref (or the source's default branch) without cloning.
    pub fn remote_head(&self) -> Result<String> {
        let refspec = self.git_ref.as_deref().unwrap_or("HEAD");
        let output = Command::new("git")
            .args(["ls-remote", &self.url, refspec])
            .output()
            .context("failed to run `git ls-remote`")?;
        if !output.status.success() {
            bail!(
                "git ls-remote {} {refspec} failed: {}",
                self.url,
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
        let stdout = String::from_utf8_lossy(&output.stdout);
        stdout
            .lines()
            .next()
            .and_then(|line| line.split_whitespace().next())
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .ok_or_else(|| anyhow::anyhow!("ref '{refspec}' not found at {}", self.url))
    }

    /// Shallow, sparse fetch of a single skill subdirectory into `dest`
    /// (created if missing, overwritten if present). Returns the commit
    /// hash checked out.
    pub fn fetch_skill(&self, name: &str, dest: &Path) -> Result<String> {
        let tmp = tempfile::tempdir().context("failed to create temp clone dir")?;
        self.shallow_clone(tmp.path(), false)?;
        run_git(tmp.path(), &["sparse-checkout", "init", "--cone"])?;
        run_git(tmp.path(), &["sparse-checkout", "set", name])?;
        run_git(tmp.path(), &["checkout"])?;

        let skill_src = tmp.path().join(name);
        if crate::skills_module::find_skill_instructions(&skill_src).is_none() {
            bail!(
                "skill '{name}' not found at {} (no SKILL.md/INSTRUCTIONS.md)",
                self.url
            );
        }

        if dest.exists() {
            std::fs::remove_dir_all(dest)
                .with_context(|| format!("failed to clear stale store dir {}", dest.display()))?;
        }
        copy_dir_recursive(&skill_src, dest)?;

        commit_hash(tmp.path())
    }

    /// Shallow fetch of the whole registry, returning every skill directory
    /// name plus its one-line description, and the commit hash fetched.
    pub fn fetch_catalog(&self) -> Result<(Vec<CatalogEntry>, String)> {
        let tmp = tempfile::tempdir().context("failed to create temp clone dir")?;
        self.shallow_clone(tmp.path(), true)?;

        let mut entries = Vec::new();
        for dir_entry in std::fs::read_dir(tmp.path())
            .context("failed to read cloned registry")?
            .flatten()
        {
            let path = dir_entry.path();
            if !path.is_dir() || path.file_name().is_some_and(|n| n == ".git") {
                continue;
            }
            let Some(instructions) = crate::skills_module::find_skill_instructions(&path) else {
                continue;
            };
            let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
                continue;
            };
            let description = std::fs::read_to_string(&instructions)
                .map(|content| super::frontmatter::parse_description(&content))
                .unwrap_or_default();
            entries.push(CatalogEntry {
                name: name.to_string(),
                description,
            });
        }
        entries.sort_by(|a, b| a.name.cmp(&b.name));

        let commit = commit_hash(tmp.path())?;
        Ok((entries, commit))
    }

    fn shallow_clone(&self, dest: &Path, checkout: bool) -> Result<()> {
        let mut cmd = Command::new("git");
        cmd.args(["clone", "--depth", "1", "--filter=blob:none"]);
        if !checkout {
            cmd.arg("--no-checkout");
        }
        if let Some(r) = &self.git_ref {
            cmd.args(["--branch", r]);
        }
        cmd.arg(&self.url).arg(dest);
        let output = cmd.output().context("failed to run `git clone`")?;
        if !output.status.success() {
            bail!(
                "git clone {} failed: {}",
                self.url,
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
        Ok(())
    }
}

fn run_git(dir: &Path, args: &[&str]) -> Result<()> {
    let output = Command::new("git")
        .current_dir(dir)
        .args(args)
        .output()
        .with_context(|| format!("failed to run `git {}`", args.join(" ")))?;
    if !output.status.success() {
        bail!(
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(())
}

fn commit_hash(dir: &Path) -> Result<String> {
    let output = Command::new("git")
        .current_dir(dir)
        .args(["rev-parse", "HEAD"])
        .output()
        .context("failed to run `git rev-parse HEAD`")?;
    if !output.status.success() {
        bail!(
            "git rev-parse HEAD failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

fn copy_dir_recursive(src: &Path, dst: &Path) -> Result<()> {
    std::fs::create_dir_all(dst)?;
    for entry in std::fs::read_dir(src)?.flatten() {
        let file_name = entry.file_name();
        if file_name == ".git" {
            continue;
        }
        let dest_path = dst.join(&file_name);
        if entry.file_type()?.is_dir() {
            copy_dir_recursive(&entry.path(), &dest_path)?;
        } else {
            std::fs::copy(entry.path(), &dest_path)?;
        }
    }
    Ok(())
}
