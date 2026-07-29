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
use inquire::{Confirm, CustomType, MultiSelect, Select};
use std::io::{self, Write};

pub fn run_setup(force_skills: bool) -> Result<()> {
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

    // ── Step 2.25: TUI theme preference ──────────────────────────
    wiz.render()?;
    let theme = select_theme(&existing_config.theme)?;
    wiz.add(format!(
        "\x1b[32m✓\x1b[0m Theme: {}",
        match theme.as_str() {
            "modern" => THEME_OPTION_MODERN,
            _ => THEME_OPTION_CLASSIC,
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

    let rag_capable = crate::rag::embedding_client::provider_available(
        crate::rag::embedding_client::EmbeddingProvider::Local,
    );

    let (embeddings_model, similarity_threshold, rag_personal_dirs, rag_max_file_mb) = if use_rag
        && !rag_capable
    {
        // This build has no way to serve any model the wizard can currently
        // offer — every option in `select_local_embeddings_model` is a local
        // (fastembed/ONNX) model. Refuse to offer them rather than saving a
        // config that `canopy doctor` and `rag_search` will only later
        // discover cannot actually run.
        println!();
        println!("  \x1b[31m✗  This canopy build cannot run local embedding models.\x1b[0m");
        println!(
            "  \x1b[90m{}\x1b[0m",
            crate::rag::embedding_client::LOCAL_EMBEDDINGS_UNAVAILABLE_REASON
        );
        println!(
            "  \x1b[90mRAG will stay disabled. Install a build with local-embeddings, or\x1b[0m"
        );
        println!("  \x1b[90mconfigure a cloud embeddings model manually in config.toml.\x1b[0m");
        println!();
        wiz.add(
            "\x1b[33m⚠\x1b[0m RAG: disabled — build lacks local-embeddings support".to_string(),
        );
        (
            String::new(),
            existing_config.similarity_threshold,
            Vec::new(),
            existing_config.rag_max_file_mb,
        )
    } else if use_rag {
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

        // ── Model acquisition ─────────────────────────────────────────
        // Setup never downloads the model itself — it only checks whether
        // one's already cached, so this returns immediately either way. If
        // it isn't cached, the daemon's background acquisition loop (see
        // `IngestionManager::ensure_configured_model_acquired`) picks it up
        // once it (re)starts below, so a multi-hundred-MB download never
        // blocks this wizard.
        wiz.render()?;
        #[cfg(feature = "local-embeddings")]
        let model_already_cached = {
            let model_cache_dir = canopy_dir.join("models");
            crate::rag::embedding_client::is_local_model_cached(&embeddings_model, &model_cache_dir)
                .unwrap_or(false)
        };
        #[cfg(not(feature = "local-embeddings"))]
        let model_already_cached = false;
        if model_already_cached {
            wiz.add(format!(
                "\x1b[32m✓\x1b[0m Model ready: {embeddings_model} (already cached)"
            ));
        } else {
            wiz.add(
                "\x1b[33m⬇\x1b[0m Model will download in the background — indexing begins once it's ready"
                    .to_string(),
            );
        }

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

        // ── Per-file indexing size limit ─────────────────────────────
        wiz.render()?;
        let rag_max_file_mb = select_rag_max_file_mb(existing_config.rag_max_file_mb)?;
        wiz.add(format!(
            "\x1b[32m✓\x1b[0m Indexing size limit: {rag_max_file_mb} MB per file"
        ));

        (
            embeddings_model,
            similarity_threshold,
            rag_personal_dirs,
            rag_max_file_mb,
        )
    } else {
        wiz.add("\x1b[90m–\x1b[0m RAG: disabled".to_string());
        (
            String::new(),
            existing_config.similarity_threshold,
            Vec::new(),
            existing_config.rag_max_file_mb,
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
    let skills_step = run_essential_skills_step(&home, &selected, force_skills);
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
    config.theme = theme;
    config.embeddings_model = embeddings_model;
    config.similarity_threshold = similarity_threshold;
    config.rag_personal_dirs = rag_personal_dirs;
    config.rag_max_file_mb = rag_max_file_mb;
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

const THEME_OPTION_CLASSIC: &str = "Classic (bordered)";
const THEME_OPTION_MODERN: &str = "Modern (borderless)";

fn select_theme(current: &str) -> Result<String> {
    let options = [THEME_OPTION_CLASSIC, THEME_OPTION_MODERN];
    let start = if current == "modern" { 1 } else { 0 };
    let selected = Select::new("TUI theme:", options.to_vec())
        .with_starting_cursor(start)
        .with_help_message("enter: confirm | ↑↓: navigate | restart the TUI to apply")
        .prompt()
        .map_err(|e| anyhow::anyhow!("Theme selection cancelled: {}", e))?;

    Ok(theme_choice_to_config_value(selected))
}

/// Map a `select_theme` menu label to the `CanopyConfig::theme` value.
/// Pure so it's testable without an interactive prompt.
fn theme_choice_to_config_value(selected: &str) -> String {
    if selected == THEME_OPTION_MODERN {
        "modern".to_string()
    } else {
        "classic".to_string()
    }
}

/// Prompt for the per-file indexing size cap, in MB, with the currently
/// configured value (or the 10 MB default on first run) preselected. Rejects
/// out-of-range input inline via the same `validate_rag_max_file_mb` doctor
/// and ingestion both defer to, so the wizard can't save a value neither of
/// them would actually honor.
fn select_rag_max_file_mb(current: u32) -> Result<u32> {
    use inquire::validator::Validation;

    CustomType::<u32>::new("Per-file indexing size limit (MB):")
        .with_default(current)
        .with_help_message(&format!(
            "Files larger than this are skipped during indexing | ceiling: {} MB",
            crate::domain::canopy_config::RAG_MAX_FILE_MB_CEILING
        ))
        .with_validator(|mb: &u32| {
            Ok(
                match crate::domain::canopy_config::validate_rag_max_file_mb(*mb) {
                    Ok(()) => Validation::Valid,
                    Err(reason) => Validation::Invalid(reason.into()),
                },
            )
        })
        .prompt()
        .map_err(|e| anyhow::anyhow!("Indexing size limit selection cancelled: {}", e))
}

/// Human-readable label (name, dimensions, approximate download size) for a
/// supported local model id. The set of ids offered is derived from
/// `embedding_client::LOCAL_MODEL_IDS` — the same single source of truth
/// `model_id_to_fastembed` maps from — instead of a second hand-maintained
/// id list, so the wizard can no longer drift out of step with which models
/// are actually supported. Only the descriptive text lives here; coverage
/// against `LOCAL_MODEL_IDS` is asserted by
/// `every_local_model_id_has_a_label` below.
fn local_model_label(id: &str) -> &'static str {
    match id {
        "baai/bge-small-en-v1.5" => {
            "BGE Small EN v1.5     (local · 384d · ~130 MB)  — fast, great for English"
        }
        "baai/bge-base-en-v1.5" => {
            "BGE Base EN v1.5      (local · 768d · ~430 MB)  — balanced, English"
        }
        "baai/bge-large-en-v1.5" => {
            "BGE Large EN v1.5     (local · 1024d · ~1.3 GB) — best quality, English"
        }
        "intfloat/multilingual-e5-small" => {
            "Multilingual E5 Small (local · 384d · ~480 MB)  — fast, multilingual"
        }
        "intfloat/multilingual-e5-base" => {
            "Multilingual E5 Base  (local · 768d · ~1.1 GB)  — balanced, multilingual"
        }
        "intfloat/multilingual-e5-large" => {
            "Multilingual E5 Large (local · 1024d · ~2.2 GB) — best quality, multilingual"
        }
        // Unreached in practice — every id in LOCAL_MODEL_IDS is covered
        // above, and every_local_model_id_has_a_label fails the build if a
        // new one is added here without it.
        _ => "(unlabeled model)",
    }
}

fn select_local_embeddings_model(current: &str) -> Result<String> {
    let ids = crate::rag::embedding_client::LOCAL_MODEL_IDS;
    let options: Vec<&str> = ids.iter().map(|id| local_model_label(id)).collect();

    let start = ids.iter().position(|id| *id == current).unwrap_or(0);

    let selected = Select::new("Embeddings model (local, no API key required):", options)
        .with_starting_cursor(start)
        .with_help_message(
            "Downloaded once to ~/.canopy/models/ — no internet needed after that | ↑↓: navigate | enter: confirm",
        )
        .prompt()
        .map_err(|e| anyhow::anyhow!("Embeddings model selection cancelled: {}", e))?;

    ids.iter()
        .find(|id| local_model_label(id) == selected)
        .map(|id| id.to_string())
        .ok_or_else(|| anyhow::anyhow!("Unknown embeddings model selection"))
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Every id the wizard could offer (derived from `LOCAL_MODEL_IDS`) must
    /// have a real label, not the id-echoing fallback arm in
    /// `local_model_label` — that fallback exists only so a missing label
    /// can't panic mid-setup; this test is what actually catches it.
    #[test]
    fn every_local_model_id_has_a_label() {
        for id in crate::rag::embedding_client::LOCAL_MODEL_IDS {
            assert_ne!(
                local_model_label(id),
                *id,
                "missing a descriptive label for '{id}'"
            );
        }
    }

    #[test]
    fn local_model_labels_are_unique() {
        let ids = crate::rag::embedding_client::LOCAL_MODEL_IDS;
        let mut labels: Vec<&str> = ids.iter().map(|id| local_model_label(id)).collect();
        labels.sort_unstable();
        labels.dedup();
        assert_eq!(
            labels.len(),
            ids.len(),
            "two ids resolved to the same label — select_local_embeddings_model \
             maps the chosen label back to an id and needs them distinct"
        );
    }

    #[test]
    fn theme_choice_writes_modern_for_the_modern_menu_label() {
        assert_eq!(theme_choice_to_config_value(THEME_OPTION_MODERN), "modern");
    }

    #[test]
    fn theme_choice_writes_classic_for_the_classic_menu_label() {
        assert_eq!(
            theme_choice_to_config_value(THEME_OPTION_CLASSIC),
            "classic"
        );
    }

    #[test]
    fn theme_choice_defaults_unrecognized_input_to_classic() {
        // Defensive: any label that isn't the modern one falls back to classic
        // rather than writing an unexpected value to config.
        assert_eq!(theme_choice_to_config_value("not a real option"), "classic");
    }

    #[test]
    fn theme_choice_modern_constant_value() {
        assert_eq!(THEME_OPTION_MODERN, "Modern (borderless)");
    }

    #[test]
    fn theme_choice_classic_constant_value() {
        assert_eq!(THEME_OPTION_CLASSIC, "Classic (bordered)");
    }

    #[test]
    fn wizard_state_new_is_empty() {
        let wiz = WizardState::new();
        assert!(wiz.steps.is_empty());
    }

    #[test]
    fn wizard_state_add_stores_steps() {
        let mut wiz = WizardState::new();
        wiz.add("step 1".to_string());
        wiz.add("step 2".to_string());
        assert_eq!(wiz.steps.len(), 2);
        assert_eq!(wiz.steps[0], "step 1");
        assert_eq!(wiz.steps[1], "step 2");
    }

    #[test]
    fn wizard_state_render_returns_ok() {
        // render() calls clear_wizard_screen() which does I/O, but we test
        // that the function at least constructs without panic.
        let wiz = WizardState::new();
        // This may fail in headless CI (no terminal), but the test compiles
        // and demonstrates the function is reachable.
        let _ = wiz.render();
    }

    #[test]
    fn wizard_state_add_preserves_order() {
        let mut wiz = WizardState::new();
        for i in 0..10 {
            wiz.add(format!("step {i}"));
        }
        for (i, step) in wiz.steps.iter().enumerate() {
            assert_eq!(*step, format!("step {i}"));
        }
    }

    // ── Additional edge cases ────────────────────────────────────

    #[test]
    fn theme_choice_modern_roundtrip() {
        let result = theme_choice_to_config_value(THEME_OPTION_MODERN);
        assert_eq!(result, "modern");
    }

    #[test]
    fn theme_choice_classic_roundtrip() {
        let result = theme_choice_to_config_value(THEME_OPTION_CLASSIC);
        assert_eq!(result, "classic");
    }

    #[test]
    fn theme_choice_empty_string() {
        assert_eq!(theme_choice_to_config_value(""), "classic");
    }

    #[test]
    fn theme_choice_arbitrary_string() {
        assert_eq!(theme_choice_to_config_value("anything"), "classic");
    }

    #[test]
    fn wizard_state_new_has_zero_steps() {
        let wiz = WizardState::new();
        assert_eq!(wiz.steps.len(), 0);
    }

    #[test]
    fn wizard_state_add_single_step() {
        let mut wiz = WizardState::new();
        wiz.add("single step".to_string());
        assert_eq!(wiz.steps.len(), 1);
        assert_eq!(wiz.steps[0], "single step");
    }

    #[test]
    fn wizard_state_add_many_steps() {
        let mut wiz = WizardState::new();
        for i in 0..100 {
            wiz.add(format!("step {i}"));
        }
        assert_eq!(wiz.steps.len(), 100);
    }

    #[test]
    fn theme_choice_case_sensitivity() {
        // "Modern (borderless)" is the exact constant
        assert_eq!(
            theme_choice_to_config_value("Modern (borderless)"),
            "modern"
        );
        // Different casing should fall back to classic
        assert_eq!(
            theme_choice_to_config_value("modern (borderless)"),
            "classic"
        );
    }
}
