use crate::setup_module::daemon_service::{
    install_service_if_needed, start_daemon_if_needed, stop_daemon,
};
use crate::setup_module::dir_browser::browse_directories_multiselect_with_preselected;
use crate::setup_module::models::{is_platform_available, Platform};
use crate::setup_module::platform_adapter::clear_wizard_screen;
use crate::setup_module::registry_fetch::{fetch_registry, print_banner};
use crate::setup_module::sync_and_skills::{run_essential_skills_step, run_sync_step};
use crate::setup_module::PlatformWithCli;
use anyhow::{Context, Result};
use inquire::{Confirm, MultiSelect, Select};
use std::io::{self, Write};

pub fn run_setup() -> Result<()> {
    let mut wiz = WizardState::new();
    let home = dirs::home_dir().context("No home directory")?;
    let canopy_dir = home.join(".canopy");
    let existing_config = crate::domain::canopy_config::CanopyConfig::load(&canopy_dir);

    // ── Step 1: Fetch registry ──────────────────────────────────
    clear_wizard_screen()?;
    print_banner();
    print!("  Fetching platform registry... ");
    io::stdout().flush()?;
    let mut registry = fetch_registry()?;

    // Legacy v5 compat: no longer needed with v6
    let _ = &mut registry;
    println!("\x1b[32m✓\x1b[0m");

    let detected: Vec<&Platform> = registry
        .platforms
        .iter()
        .filter(|p| is_platform_available(p))
        .collect();

    let detected_names: Vec<&str> = detected.iter().map(|p| p.name.as_str()).collect();
    wiz.add(format!(
        "\x1b[32m✓\x1b[0m Fetched registry — {} detected: {}",
        detected.len(),
        if detected_names.is_empty() {
            "(none)".to_string()
        } else {
            detected_names.join(", ")
        }
    ));

    // ── Step 2: Select platforms ─────────────────────────────────
    wiz.render()?;
    if detected.is_empty() {
        println!(
            "  No supported platforms detected. Supported: {}",
            registry
                .platforms
                .iter()
                .map(|p| p.name.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        );
        println!();
    }

    let selected = select_platforms(&detected)?;
    let selected_names: Vec<&str> = selected.iter().map(|p| p.name.as_str()).collect();
    wiz.add(format!(
        "\x1b[32m✓\x1b[0m Platforms: {}",
        if selected_names.is_empty() {
            "(none)".to_string()
        } else {
            selected_names.join(", ")
        }
    ));

    // ── Step 2.2: Temperature unit preference ────────────────────
    wiz.render()?;
    let temperature_unit = select_temperature_unit()?;
    wiz.add(format!(
        "\x1b[32m✓\x1b[0m Temperature unit: {}",
        match temperature_unit {
            crate::domain::canopy_config::TemperatureUnit::Celsius => "Celsius (°C)",
            crate::domain::canopy_config::TemperatureUnit::Fahrenheit => "Fahrenheit (°F)",
        }
    ));

    // ── Step 2.3: RAG opt-in ─────────────────────────────────────
    wiz.render()?;
    let rag_previously_configured = !existing_config.embeddings_model.is_empty()
        || !existing_config.rag_personal_dirs.is_empty();
    let use_rag = Confirm::new("Enable personal knowledge indexing (RAG)?")
        .with_default(rag_previously_configured)
        .with_help_message(
            "Indexes your notes/docs so AI tools can search them — runs fully locally",
        )
        .prompt()
        .map_err(|e| anyhow::anyhow!("RAG selection cancelled: {}", e))?;

    let (embeddings_model, similarity_threshold, rag_personal_dirs) = if use_rag {
        // ── Select local embedding model ──────────────────────────
        wiz.render()?;
        let embeddings_model = select_local_embeddings_model(&existing_config.embeddings_model)?;

        // Warn if model changed — re-indexing all documents will be required.
        if !existing_config.embeddings_model.is_empty()
            && embeddings_model != existing_config.embeddings_model
        {
            println!();
            println!("  \x1b[33m⚠  Embeddings model changed.\x1b[0m");
            println!(
                "  \x1b[90mAll previously indexed documents will need to be re-indexed.\x1b[0m"
            );
            println!("  \x1b[90mThis is a heavy operation and may take a while.\x1b[0m");
            println!();
            let confirmed = Confirm::new("Continue with the new model?")
                .with_default(false)
                .with_help_message("enter: confirm")
                .prompt()
                .unwrap_or(false);
            if !confirmed {
                anyhow::bail!("Embeddings model change cancelled by user");
            }
        }

        wiz.add(format!(
            "\x1b[32m✓\x1b[0m Embeddings model: {}",
            embeddings_model
        ));

        // ── Download / warm-up the model ──────────────────────────
        wiz.render()?;
        let model_cache_dir = canopy_dir.join("models");
        download_local_model_for_setup(&embeddings_model, &model_cache_dir)?;
        wiz.add(format!(
            "\x1b[32m✓\x1b[0m Model ready: {}",
            embeddings_model
        ));

        // Chunk-merge similarity threshold: internal tuning knob with no
        // user-observable effect in its valid range, so it is not prompted.
        // The config.toml value (default 0.4) is carried forward and can
        // still be edited manually for experimentation.
        let similarity_threshold = existing_config.similarity_threshold;

        // ── RAG directories ─────────────────────────────────────────
        let prev_dirs = existing_config.rag_personal_dirs.clone();
        // Start browser at the parent of the first configured dir so the user
        // sees their selection highlighted instead of landing inside an empty dir.
        let browser_start = if !prev_dirs.is_empty() {
            prev_dirs
                .first()
                .and_then(|p| {
                    std::path::Path::new(p)
                        .parent()
                        .map(|pp| pp.to_string_lossy().to_string())
                })
                .filter(|pp| std::path::Path::new(pp).is_dir())
                .unwrap_or_else(|| prev_dirs.first().unwrap().clone())
        } else if !existing_config.rag_personal_root.is_empty() {
            std::path::Path::new(&existing_config.rag_personal_root)
                .parent()
                .map(|pp| pp.to_string_lossy().to_string())
                .filter(|pp| std::path::Path::new(pp).is_dir())
                .unwrap_or(existing_config.rag_personal_root.clone())
        } else {
            String::new()
        };
        let rag_personal_dirs = pick_multiple_directories(
            "Personal RAG directories (your own notes/docs — indexed for global retrieval):",
            &browser_start,
            &prev_dirs,
        )?;
        wiz.add(format!(
            "\x1b[32m✓\x1b[0m Personal RAG dirs: {}",
            rag_personal_dirs.join(", ")
        ));

        (embeddings_model, similarity_threshold, rag_personal_dirs)
    } else {
        wiz.add("\x1b[90m–\x1b[0m RAG: disabled".to_string());
        (
            String::new(),
            existing_config.similarity_threshold,
            Vec::new(),
        )
    };

    // ── Step 3: Install MCP servers + show matrix ───────────────
    if !selected.is_empty() {
        let sync_summary = run_sync_step(&mut wiz, &home, &selected, &registry.canonical_servers)?;
        if let Some(s) = sync_summary {
            wiz.add(s);
        }
    }

    // ── Step 4: Save CLI configuration ──────────────────────────
    let platforms_with_cli: Vec<PlatformWithCli> = selected
        .iter()
        .map(|p| p.to_platform_with_cli())
        .filter(|p| p.cli.is_some())
        .collect();

    let cli_registry =
        crate::domain::cli_config::CliRegistry::detect_available(&platforms_with_cli);
    std::fs::create_dir_all(&canopy_dir)?;
    for dir in &rag_personal_dirs {
        std::fs::create_dir_all(dir)?;
    }

    // ── Step 5: Essential Skills ─────────────────────────────────
    wiz.render()?;
    let skills_step = run_essential_skills_step(&home, &selected);
    wiz.add(skills_step);

    // ── Step 6: Daemon + service ────────────────────────────────
    wiz.render()?;

    // Always restart daemon to pick up new MCP configs
    let _ = stop_daemon();
    let daemon_msg = match start_daemon_if_needed() {
        Ok(true) => "\x1b[32m✓\x1b[0m Daemon: (re)started",
        Ok(false) => "\x1b[32m✓\x1b[0m Daemon: already running",
        Err(_) => "\x1b[31m✗\x1b[0m Daemon: failed to start",
    };
    wiz.add(daemon_msg.to_string());

    let service_msg = match install_service_if_needed() {
        Ok(true) => "\x1b[32m✓\x1b[0m Service: installed",
        Ok(false) => "\x1b[32m✓\x1b[0m Service: already installed",
        Err(_) => "\x1b[31m✗\x1b[0m Service: failed to install",
    };
    wiz.add(service_msg.to_string());

    // ── Save unified config ──────────────────────────────────────
    let mut config = crate::domain::canopy_config::CanopyConfig::load(&canopy_dir);
    config.mark_configured();
    config.clis = cli_registry.available_clis;
    config.temperature_unit = temperature_unit;
    config.embeddings_model = embeddings_model;
    config.similarity_threshold = similarity_threshold;
    config.rag_personal_dirs = rag_personal_dirs;
    let config_step = match config.save(&canopy_dir) {
        Ok(_) => format!(
            "\x1b[32m✓\x1b[0m Config: {} CLI(s) saved to config.toml",
            config.clis.len()
        ),
        Err(e) => format!("\x1b[33m⚠\x1b[0m Config: {e}"),
    };
    wiz.add(config_step);

    // ── Final summary ───────────────────────────────────────────
    wiz.render()?;
    println!("  \x1b[1;32m✅ Setup complete! canopy is ready.\x1b[0m");
    println!("  Run \x1b[1mcanopy\x1b[0m or \x1b[1mcanopy tui\x1b[0m to launch the interface.");
    println!();

    Ok(())
}
/// Tracks completed wizard steps so we can re-render a clean summary
/// after clearing the screen between interactive phases.
pub(crate) struct WizardState {
    steps: Vec<String>,
}

impl WizardState {
    fn new() -> Self {
        Self { steps: vec![] }
    }

    fn add(&mut self, summary: String) {
        self.steps.push(summary);
    }

    /// Clear screen → banner → all completed step summaries.
    pub(crate) fn render(&self) -> Result<()> {
        clear_wizard_screen()?;
        print_banner();
        for step in &self.steps {
            println!("  {step}");
        }
        if !self.steps.is_empty() {
            println!();
        }
        Ok(())
    }
}

fn select_platforms<'a>(detected: &[&'a Platform]) -> Result<Vec<&'a Platform>> {
    if detected.is_empty() {
        println!("  Press Enter to continue...");
        let mut buf = String::new();
        io::stdin().read_line(&mut buf)?;
        return Ok(vec![]);
    }

    let platform_names: Vec<&str> = detected.iter().map(|p| p.name.as_str()).collect();
    let all_indices: Vec<usize> = (0..detected.len()).collect();

    let selected = MultiSelect::new("Select platforms to configure:", platform_names)
        .with_default(&all_indices)
        .with_help_message("space: toggle | enter: confirm | ↑↓: navigate")
        .prompt()
        .map_err(|e| anyhow::anyhow!("Selection cancelled: {}", e))?;

    Ok(selected
        .iter()
        .filter_map(|name| detected.iter().find(|p| p.name == *name).copied())
        .collect())
}

fn select_temperature_unit() -> Result<crate::domain::canopy_config::TemperatureUnit> {
    let options = ["Celsius (°C)", "Fahrenheit (°F)"];
    let selected = Select::new("Temperature unit for sysinfo:", options.to_vec())
        .with_starting_cursor(0)
        .with_help_message("enter: confirm | ↑↓: navigate")
        .prompt()
        .map_err(|e| anyhow::anyhow!("Temperature selection cancelled: {}", e))?;

    Ok(match selected {
        "Fahrenheit (°F)" => crate::domain::canopy_config::TemperatureUnit::Fahrenheit,
        _ => crate::domain::canopy_config::TemperatureUnit::Celsius,
    })
}

fn select_local_embeddings_model(current: &str) -> Result<String> {
    const LOCAL_MODELS: &[(&str, &str)] = &[
        (
            "baai/bge-small-en-v1.5",
            "BGE Small EN v1.5     (local · 384d · ~130 MB)  — fast, great for English",
        ),
        (
            "baai/bge-base-en-v1.5",
            "BGE Base EN v1.5      (local · 768d · ~430 MB)  — balanced, English",
        ),
        (
            "baai/bge-large-en-v1.5",
            "BGE Large EN v1.5     (local · 1024d · ~1.3 GB) — best quality, English",
        ),
        (
            "intfloat/multilingual-e5-small",
            "Multilingual E5 Small (local · 384d · ~480 MB)  — fast, multilingual",
        ),
        (
            "intfloat/multilingual-e5-base",
            "Multilingual E5 Base  (local · 768d · ~1.1 GB)  — balanced, multilingual",
        ),
        (
            "intfloat/multilingual-e5-large",
            "Multilingual E5 Large (local · 1024d · ~2.2 GB) — best quality, multilingual",
        ),
    ];

    let options: Vec<&str> = LOCAL_MODELS.iter().map(|(_, label)| *label).collect();

    let start = LOCAL_MODELS
        .iter()
        .position(|(id, _)| *id == current)
        .unwrap_or(0);

    let selected = Select::new("Embeddings model (local, no API key required):", options)
        .with_starting_cursor(start)
        .with_help_message(
            "Downloaded once to ~/.canopy/models/ — no internet needed after that | ↑↓: navigate | enter: confirm",
        )
        .prompt()
        .map_err(|e| anyhow::anyhow!("Embeddings model selection cancelled: {}", e))?;

    LOCAL_MODELS
        .iter()
        .find(|(_, label)| *label == selected)
        .map(|(id, _)| id.to_string())
        .ok_or_else(|| anyhow::anyhow!("Unknown embeddings model selection"))
}

fn download_local_model_for_setup(model_id: &str, cache_dir: &std::path::Path) -> Result<()> {
    println!("  \x1b[90mDownloading model to ~/.canopy/models/ (only needed once)…\x1b[0m");
    println!();
    crate::rag::embedding_client::download_local_model(model_id, cache_dir)?;
    println!();
    Ok(())
}

/// Interactively pick one or more directories for personal RAG indexing.
fn pick_multiple_directories(
    message: &str,
    initial: &str,
    existing: &[String],
) -> Result<Vec<String>> {
    println!("  {message}");
    if !existing.is_empty() {
        println!("  \x1b[90mCurrently configured:\x1b[0m");
        for dir in existing {
            println!("    \x1b[90m• {dir}\x1b[0m");
        }
    }
    println!("  \x1b[90mUse Space to mark directories, navigate with ↑↓, → to enter folders, ← to go up, Enter to confirm.\x1b[0m");

    let pre_selected: std::collections::HashSet<String> = existing.iter().cloned().collect();
    let selected = browse_directories_multiselect_with_preselected(initial, pre_selected);

    if selected.is_empty() {
        anyhow::bail!("At least one RAG directory is required");
    }

    // Sort for consistency
    let mut dirs = selected;
    dirs.sort();

    println!("\n  \x1b[32m✓\x1b[0m Selected directories:");
    for dir in &dirs {
        println!("    \x1b[90m• {dir}\x1b[0m");
    }
    println!();

    Ok(dirs)
}
