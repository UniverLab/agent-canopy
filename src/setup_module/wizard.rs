use crate::setup_module::daemon_service::{
    install_service_if_needed, start_daemon_if_needed, stop_daemon,
};
use crate::setup_module::models::{is_platform_available, Platform};
use crate::setup_module::platform_adapter::{browse_directories_multiselect, clear_wizard_screen};
use crate::setup_module::registry_fetch::{fetch_registry, print_banner};
use crate::setup_module::sync_and_skills::{run_essential_skills_step, run_sync_step};
use crate::setup_module::PlatformWithCli;
use anyhow::{Context, Result};
use inquire::{Confirm, MultiSelect, Select, Text};
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

    // ── Step 2.3: Knowledge layer preferences ───────────────────
    wiz.render()?;
    let embeddings_model = select_embeddings_model(&existing_config.embeddings_model)?;

    // Warn if model changed — re-indexing all documents will be required.
    if !existing_config.embeddings_model.is_empty()
        && embeddings_model != existing_config.embeddings_model
    {
        println!();
        println!("  \x1b[33m⚠  Embeddings model changed.\x1b[0m");
        println!("  \x1b[90mAll previously indexed documents will need to be re-indexed.\x1b[0m");
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

    wiz.render()?;
    let prev_dirs = existing_config.rag_personal_dirs.clone();
    let rag_personal_dirs = pick_multiple_directories(
        "Personal RAG directories (your own notes/docs — indexed for global retrieval):",
        if prev_dirs.is_empty() {
            &existing_config.rag_personal_root
        } else {
            prev_dirs.first().map(String::as_str).unwrap_or("")
        },
        &prev_dirs,
    )?;
    wiz.add(format!(
        "\x1b[32m✓\x1b[0m Personal RAG dirs: {}",
        rag_personal_dirs.join(", ")
    ));

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

fn select_embeddings_model(current: &str) -> Result<String> {
    let mut models = crate::domain::models_db::load_catalog()
        .map(|catalog| {
            catalog
                .models
                .into_iter()
                .filter(|model| {
                    let id = model.id.to_lowercase();
                    let name = model.name.to_lowercase();
                    id.contains("embed") || name.contains("embed")
                })
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();

    if models.is_empty() {
        models = vec![
            crate::domain::models_db::ModelEntry {
                id: "text-embedding-3-small".to_string(),
                name: "text-embedding-3-small".to_string(),
                provider: "openai".to_string(),
                release_date: Some("2024-01-25".to_string()),
                size_hint: Some("small".to_string()),
            },
            crate::domain::models_db::ModelEntry {
                id: "text-embedding-3-large".to_string(),
                name: "text-embedding-3-large".to_string(),
                provider: "openai".to_string(),
                release_date: Some("2024-01-25".to_string()),
                size_hint: Some("large".to_string()),
            },
            crate::domain::models_db::ModelEntry {
                id: "gemini-embedding-001".to_string(),
                name: "Gemini Embedding 001".to_string(),
                provider: "google".to_string(),
                release_date: Some("2025-05-20".to_string()),
                size_hint: None,
            },
        ];
    }

    if !current.is_empty() && !models.iter().any(|model| model.id == current) {
        models.insert(
            0,
            crate::domain::models_db::ModelEntry {
                id: current.to_string(),
                name: current.to_string(),
                provider: "custom".to_string(),
                release_date: None,
                size_hint: None,
            },
        );
    }

    models.dedup_by(|a, b| a.id == b.id);
    // Sort newest-first (lexicographic descending on date, None last).
    models.sort_by(|a, b| {
        let da = a.release_date.as_deref().unwrap_or("");
        let db = b.release_date.as_deref().unwrap_or("");
        db.cmp(da)
    });

    let mut options = models
        .iter()
        .map(format_embeddings_option)
        .collect::<Vec<_>>();
    options.push("Custom…".to_string());

    let start = models
        .iter()
        .position(|model| model.id == current)
        .unwrap_or(0);
    let selected = Select::new("Embeddings model for the knowledge layer:", options)
        .with_starting_cursor(start)
        .with_help_message("enter: confirm | ↑↓: navigate")
        .prompt()
        .map_err(|e| anyhow::anyhow!("Embeddings model selection cancelled: {}", e))?;

    if selected == "Custom…" {
        Text::new("Custom embeddings model:")
            .with_initial_value(current)
            .with_help_message("enter: confirm")
            .prompt()
            .map_err(|e| anyhow::anyhow!("Custom embeddings model cancelled: {}", e))
    } else {
        models
            .into_iter()
            .find(|model| format_embeddings_option(model) == selected)
            .map(|model| model.id)
            .ok_or_else(|| anyhow::anyhow!("Unknown embeddings model selection"))
    }
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

    let selected = browse_directories_multiselect(initial);

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

fn format_embeddings_option(model: &crate::domain::models_db::ModelEntry) -> String {
    let release_date = model.release_date.as_deref().unwrap_or("unknown date");
    // Show model id with date only — no size hints.
    format!("{}  \x1b[90m[{}]\x1b[0m", model.id, release_date)
}
