//! CLI handlers for `canopy prompts` subcommands (P1) — discovery for the
//! file-backed prompt presets under `~/.canopy/prompts/`. Read-only, mirrors
//! `canopy loop list/info` in spirit and structure (see `loop_cli.rs`).

use std::path::Path;

use anyhow::{anyhow, Result};
use clap::Subcommand;

use crate::domain::prompts::{prompts_dir, seed_builtin_prompt_presets};

#[derive(Subcommand, Debug)]
pub(crate) enum PromptsAction {
    /// List every prompt preset with its first non-empty line.
    List,
    /// Show the full content of a prompt preset.
    Show {
        /// Preset name — the file's stem under ~/.canopy/prompts/ (e.g. "implementer").
        name: String,
    },
}

pub(crate) async fn handle_prompts_action(action: PromptsAction) -> Result<()> {
    let canopy_dir = crate::ensure_data_dir()?;
    // Idempotent and never overwrites an existing file (see
    // `seed_builtin_prompt_presets`), so this makes `canopy prompts` work
    // even before the daemon has ever started.
    if let Err(e) = seed_builtin_prompt_presets(&canopy_dir) {
        tracing::warn!("Could not seed builtin prompt presets: {e}");
    }
    let dir = prompts_dir(&canopy_dir);

    match action {
        PromptsAction::List => handle_prompts_list(&dir),
        PromptsAction::Show { name } => handle_prompts_show(&dir, &name),
    }
}

fn handle_prompts_list(dir: &Path) -> Result<()> {
    let mut names = list_preset_names(dir)?;
    if names.is_empty() {
        println!("No prompt presets found.");
        return Ok(());
    }
    names.sort();

    println!("\n\x1b[1m── Prompt Presets ─────────────────────────────────────────────\x1b[0m\n");
    for name in &names {
        let content = std::fs::read_to_string(dir.join(format!("{name}.md"))).unwrap_or_default();
        let preview = first_non_empty_line(&content).unwrap_or("");
        println!(" {name:<14} {preview}");
    }
    println!();
    Ok(())
}

fn handle_prompts_show(dir: &Path, name: &str) -> Result<()> {
    let path = dir.join(format!("{name}.md"));
    let content = std::fs::read_to_string(&path)
        .map_err(|_| anyhow!("Prompt preset '{name}' not found at {}.", path.display()))?;
    println!("\n\x1b[1m── Prompt Preset: {name} ──\x1b[0m\n");
    println!("{content}");
    Ok(())
}

fn list_preset_names(dir: &Path) -> Result<Vec<String>> {
    if !dir.exists() {
        return Ok(Vec::new());
    }
    let mut names = Vec::new();
    for entry in std::fs::read_dir(dir)? {
        let path = entry?.path();
        if path.extension().and_then(|e| e.to_str()) != Some("md") {
            continue;
        }
        if let Some(stem) = path.file_stem().and_then(|s| s.to_str()) {
            names.push(stem.to_string());
        }
    }
    Ok(names)
}

fn first_non_empty_line(content: &str) -> Option<&str> {
    content.lines().map(str::trim).find(|line| !line.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[derive(Parser)]
    struct TestCli {
        #[command(subcommand)]
        action: PromptsAction,
    }

    #[test]
    fn list_parses() {
        let cli = TestCli::try_parse_from(["test", "list"]).expect("list should parse");
        assert!(matches!(cli.action, PromptsAction::List));
    }

    #[test]
    fn show_requires_name() {
        assert!(TestCli::try_parse_from(["test", "show"]).is_err());
        let cli = TestCli::try_parse_from(["test", "show", "implementer"]).expect("should parse");
        match cli.action {
            PromptsAction::Show { name } => assert_eq!(name, "implementer"),
            other => panic!("expected Show, got {other:?}"),
        }
    }

    #[test]
    fn first_non_empty_line_skips_leading_blank_lines() {
        assert_eq!(first_non_empty_line("\n\n  hello\nworld"), Some("hello"));
        assert_eq!(first_non_empty_line(""), None);
        assert_eq!(first_non_empty_line("   \n  "), None);
    }

    #[test]
    fn list_preset_names_reads_md_files_only() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("implementer.md"), "content").unwrap();
        std::fs::write(dir.path().join("reviewer.md"), "content").unwrap();
        std::fs::write(dir.path().join("notes.txt"), "ignored").unwrap();

        let mut names = list_preset_names(dir.path()).unwrap();
        names.sort();
        assert_eq!(names, vec!["implementer", "reviewer"]);
    }

    #[test]
    fn list_preset_names_returns_empty_for_missing_dir() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("does-not-exist");
        assert_eq!(list_preset_names(&missing).unwrap(), Vec::<String>::new());
    }

    #[test]
    fn first_non_empty_line_returns_first_line() {
        assert_eq!(first_non_empty_line("hello\nworld"), Some("hello"));
        assert_eq!(first_non_empty_line("single"), Some("single"));
    }

    #[test]
    fn first_non_empty_line_trims_whitespace() {
        assert_eq!(first_non_empty_line("  hello  \nworld"), Some("hello"));
        assert_eq!(first_non_empty_line("\t\tindented\n"), Some("indented"));
    }

    #[test]
    fn first_non_empty_line_handles_only_whitespace() {
        assert_eq!(first_non_empty_line("   \n\t\n  "), None);
        assert_eq!(first_non_empty_line(""), None);
    }

    #[test]
    fn list_preset_names_returns_all_md_files() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("zebra.md"), "content").unwrap();
        std::fs::write(dir.path().join("alpha.md"), "content").unwrap();
        std::fs::write(dir.path().join("middle.md"), "content").unwrap();

        let mut names = list_preset_names(dir.path()).unwrap();
        names.sort();
        assert_eq!(names, vec!["alpha", "middle", "zebra"]);
    }

    #[test]
    fn list_preset_names_includes_directories_with_md_extension() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("file.md"), "content").unwrap();
        std::fs::create_dir(dir.path().join("directory.md")).unwrap();

        let mut names = list_preset_names(dir.path()).unwrap();
        names.sort();
        assert_eq!(names, vec!["directory", "file"]);
    }
}
