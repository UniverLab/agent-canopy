//! Interactive skills management wizard — list, validate, and remove skills.

#![allow(dead_code)]

use anyhow::Result;
use inquire::Select;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use super::{find_broken_symlinks, list_skill_dirs};

const LIST_ACTION: &str = "List — show installed skills";
const VALIDATE_ACTION: &str = "Validate — check symlink integrity";
const REMOVE_ACTION: &str = "Remove — uninstall a skill";
const SKILL_ACTIONS: [&str; 3] = [LIST_ACTION, VALIDATE_ACTION, REMOVE_ACTION];

pub fn run_skills_wizard(home: &Path, platforms: &[&crate::setup_module::Platform]) -> Result<()> {
    print_skills_wizard_header();

    let global = super::global_skills_dir_for(home);
    let action = prompt_skills_action()?;
    handle_skills_action(action, home, &global, platforms)
}

fn print_skills_wizard_header() {
    println!();
    println!(" \x1b[1mSkills Manager\x1b[0m");
    println!(" ─────────────────────────────────────────────");
}

fn prompt_skills_action() -> Result<&'static str> {
    Select::new("What would you like to do?", SKILL_ACTIONS.to_vec())
        .with_help_message("↑↓ navigate | Enter select | Esc cancel")
        .prompt()
        .map_err(|error| anyhow::anyhow!("Cancelled: {}", error))
}

fn handle_skills_action(
    action: &str,
    home: &Path,
    global: &Path,
    platforms: &[&crate::setup_module::Platform],
) -> Result<()> {
    match action {
        LIST_ACTION => list_skills(global),
        VALIDATE_ACTION => validate_skills(home, platforms),
        REMOVE_ACTION => remove_skill(home, global, platforms),
        _ => Ok(()),
    }
}

pub(super) fn list_skills(global: &Path) -> Result<()> {
    let skills = list_skill_dirs(global);
    if skills.is_empty() {
        println!(
            " \x1b[33m⚠\x1b[0m No skills installed in {}",
            global.display()
        );
        println!(" Run \x1b[1mcanopy setup\x1b[0m to download the Essential Pack.");
    } else {
        println!(" Installed skills ({}):", skills.len());
        for skill in &skills {
            println!(" \x1b[32m•\x1b[0m {skill}");
        }
    }
    Ok(())
}

pub(super) fn validate_skills(
    home: &Path,
    platforms: &[&crate::setup_module::Platform],
) -> Result<()> {
    let broken = find_broken_symlinks(home, platforms);
    if broken.is_empty() {
        println!(" \x1b[32m✓\x1b[0m All skill symlinks are healthy.");
        return Ok(());
    }

    print_broken_symlinks(&broken);
    if !prompt_yes_no(" Remove broken symlinks? [Y/n] ")? {
        return Ok(());
    }

    remove_paths(&broken);
    println!(
        " \x1b[32m✓\x1b[0m Removed {} broken symlink(s).",
        broken.len()
    );
    Ok(())
}

fn remove_skill(
    home: &Path,
    global: &Path,
    platforms: &[&crate::setup_module::Platform],
) -> Result<()> {
    let Some(selected) = prompt_skill_selection(global)? else {
        return Ok(());
    };
    if !prompt_yes_no(&format!(
        " Remove \x1b[1m{selected}\x1b[0m and all its platform symlinks? [Y/n] "
    ))? {
        println!(" Cancelled.");
        return Ok(());
    }

    remove_skill_installation(home, global, platforms, &selected);
    println!(" \x1b[32m✓\x1b[0m '{selected}' removed.");
    Ok(())
}

fn prompt_skill_selection(global: &Path) -> Result<Option<String>> {
    let skills = list_skill_dirs(global);
    if skills.is_empty() {
        println!(" \x1b[33m⚠\x1b[0m No skills to remove.");
        return Ok(None);
    }

    Select::new("Select skill to remove:", skills)
        .with_help_message("This removes the master copy and all platform symlinks")
        .prompt()
        .map(Some)
        .map_err(|error| anyhow::anyhow!("Cancelled: {}", error))
}

fn remove_skill_installation(
    home: &Path,
    global: &Path,
    platforms: &[&crate::setup_module::Platform],
    selected: &str,
) {
    let _ = std::fs::remove_dir_all(global.join(selected));

    for (_, platform_skills) in super::platform_skill_dirs(home, platforms) {
        remove_skill_path(&platform_skills.join(selected));
    }
}

fn remove_skill_path(path: &Path) {
    if !(path.exists() || path.is_symlink()) {
        return;
    }

    let _ = if path.is_symlink() || path.is_file() {
        std::fs::remove_file(path)
    } else {
        std::fs::remove_dir_all(path)
    };
}

fn prompt_yes_no(prompt: &str) -> Result<bool> {
    print!("{prompt}");
    io::stdout().flush()?;

    let mut input = String::new();
    io::stdin().read_line(&mut input)?;
    Ok(!matches!(input.trim(), "n" | "N"))
}

fn print_broken_symlinks(broken: &[PathBuf]) {
    println!(" \x1b[31m✗\x1b[0m Broken symlinks ({}):", broken.len());
    for path in broken {
        println!(" \x1b[31m✗\x1b[0m {}", path.display());
    }
}

fn remove_paths(paths: &[PathBuf]) {
    for path in paths {
        let _ = std::fs::remove_file(path);
    }
}
