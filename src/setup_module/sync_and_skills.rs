use crate::setup_module::models::{CanonicalServers, Platform};
use crate::setup_module::platform_adapter::{
    extract_all_mcp_configs, print_mcp_matrix, run_install_our_servers,
};
use crate::setup_module::wizard::WizardState;
use anyhow::Result;
use std::path::Path;

/// Run the interactive MCP setup/management step.
pub(crate) fn run_sync_step(
    wiz: &mut WizardState,
    home: &Path,
    selected: &[&Platform],
    canonical: &CanonicalServers,
) -> Result<Option<String>> {
    if selected.is_empty() {
        return Ok(None);
    }

    wiz.render()?;
    println!("  \x1b[1mMCP Manager\x1b[0m");
    println!("  ─────────────────────────────────────────────");
    println!();

    run_install_our_servers(home, selected, canonical)?;

    let all_configs = extract_all_mcp_configs(home, selected);
    if !all_configs.is_empty() {
        print_mcp_matrix(&all_configs);
    }

    Ok(Some("\x1b[32m✓\x1b[0m MCP servers updated".to_string()))
}

/// Download the UniverLab Essential Skills pack and create platform symlinks.
///
/// Runs silently on failure so a network error never blocks setup completion.
pub(crate) fn run_essential_skills_step(home: &Path, selected: &[&Platform]) -> String {
    // Ensure global skills directory exists
    if crate::skills_module::ensure_global_skills_dir().is_err() {
        return "\x1b[33m⚠\x1b[0m Skills: could not create ~/.agents/skills/".to_string();
    }

    // Download Essential Pack from GitHub (best-effort)
    let downloaded = crate::skills_module::download_essential_pack().unwrap_or_else(|e| {
        tracing::warn!("Essential skills download failed: {e}");
        0
    });

    // Create platform symlinks for all selected platforms that have skills_dir
    let symlinks =
        crate::skills_module::create_platform_symlinks(home, selected).unwrap_or_else(|e| {
            tracing::warn!("Skills symlink creation failed: {e}");
            vec![]
        });

    if downloaded == 0 && symlinks.is_empty() {
        // Check if we actually have any skills installed
        let global_dir = dirs::home_dir()
            .map(|h| h.join(".agents/skills"))
            .unwrap_or_default();
        let has_skills = global_dir.exists()
            && std::fs::read_dir(&global_dir)
                .map(|mut d| d.next().is_some())
                .unwrap_or(false);
        if has_skills {
            "\x1b[32m✓\x1b[0m Skills: up to date".to_string()
        } else {
            "\x1b[33m⚠\x1b[0m Skills: no packs available (repo not found)".to_string()
        }
    } else if downloaded > 0 {
        format!(
            "\x1b[32m✓\x1b[0m Skills: {} pack(s) downloaded, {} symlink(s) created",
            downloaded,
            symlinks.len()
        )
    } else {
        format!(
            "\x1b[32m✓\x1b[0m Skills: {} symlink(s) created",
            symlinks.len()
        )
    }
}
