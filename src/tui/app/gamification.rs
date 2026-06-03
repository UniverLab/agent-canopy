//! Gamification hooks on `App`.

use std::path::Path;
use std::sync::Arc;

use anyhow::Result;

use crate::application::ports::StateRepository;
use crate::domain::canopy_config::CanopyConfig;
use crate::domain::usage_stats::CliUsage;
use crate::tui::agent::AgentStatus;
use crate::tui::gamification::{MissionEvent, MissionManager, MissionSnapshot};

use super::types::App;

const GARDENER_EDITS_KEY: &str = "gamification:gardener_edits";
const IDENTITY_EVOLVED_KEY: &str = "gamification:identity_evolved";
const MAX_CPU_FREQ_KEY: &str = "gamification:max_cpu_freq_mhz";

/// Detect primary language from marker files at project root.
pub fn detect_project_language(path: &Path) -> Option<&'static str> {
    const MARKERS: &[(&str, &str)] = &[
        ("Cargo.toml", "rust"),
        ("go.mod", "go"),
        ("package.json", "javascript"),
        ("pnpm-lock.yaml", "javascript"),
        ("requirements.txt", "python"),
        ("pyproject.toml", "python"),
        ("Gemfile", "ruby"),
        ("pom.xml", "java"),
        ("build.gradle", "kotlin"),
        ("CMakeLists.txt", "cpp"),
        ("mix.exs", "elixir"),
    ];
    for (file, lang) in MARKERS {
        if path.join(file).exists() {
            return Some(lang);
        }
    }
    None
}

impl App {
    pub(super) fn init_mission_manager(db: Arc<crate::db::Database>) -> Result<MissionManager> {
        MissionManager::load(db)
    }

    pub fn queue_mission_event(&mut self, event: MissionEvent) {
        self.mission_pending_events.push(event);
    }

    pub fn record_gardener_edit(&mut self) -> Result<()> {
        let current = self
            .db
            .get_state(GARDENER_EDITS_KEY)?
            .and_then(|v| v.parse().ok())
            .unwrap_or(0);
        self.db
            .set_state(GARDENER_EDITS_KEY, &(current + 1).to_string())?;
        Ok(())
    }

    pub(super) fn session_args_contain_yolo(&self, session_id: &str) -> bool {
        self.db
            .get_interactive_session_args(session_id)
            .ok()
            .flatten()
            .is_some_and(|args| args.contains("yolo"))
    }

    pub(super) fn tick_missions(&mut self) -> Result<()> {
        let events: Vec<_> = self.mission_pending_events.drain(..).collect();
        let snapshot = self.build_mission_snapshot()?;
        self.mission_manager
            .process_refresh(&snapshot, &events, &mut self.whimsg)?;
        Ok(())
    }

    fn build_mission_snapshot(&mut self) -> Result<MissionSnapshot> {
        let canopy_dir = dirs::home_dir()
            .map(|h| h.join(".canopy"))
            .unwrap_or_default();
        let config = CanopyConfig::load(&canopy_dir);
        let registered_harness_count = registered_harness_count(&config, &canopy_dir);
        let harnesses_used_count = harnesses_used_count(&self.cli_usage, registered_harness_count);

        let mut languages = std::collections::HashSet::new();
        for project in &self.projects {
            if let Some(lang) = detect_project_language(Path::new(&project.path)) {
                languages.insert(lang);
            }
        }

        let gardener_edits = self
            .db
            .get_state(GARDENER_EDITS_KEY)?
            .and_then(|v| v.parse().ok())
            .unwrap_or(0);

        if self.db.get_state(IDENTITY_EVOLVED_KEY)?.as_deref() == Some("1") {
            self.queue_mission_event(MissionEvent::IdentityEvolved);
            let _ = self.db.set_state(IDENTITY_EVOLVED_KEY, "done");
        }

        let freq = self.system_info.cpu_frequency_mhz;
        if let Some(f) = freq {
            let stored = self
                .db
                .get_state(MAX_CPU_FREQ_KEY)?
                .and_then(|v| v.parse().ok())
                .unwrap_or(0);
            let max = stored.max(f);
            if max > stored {
                let _ = self.db.set_state(MAX_CPU_FREQ_KEY, &max.to_string());
            }
            self.max_cpu_frequency_seen = Some(max);
        } else {
            self.max_cpu_frequency_seen = self
                .db
                .get_state(MAX_CPU_FREQ_KEY)?
                .and_then(|v| v.parse().ok());
        }

        let gpu = self.system_info.gpu_info.as_ref();
        let agent_running = self.has_active_agent_work();

        Ok(MissionSnapshot {
            canopy_uptime_secs: self.cli_usage.canopy_uptime_seconds(),
            interactive_sessions: self.db.count_interactive_sessions().unwrap_or(0),
            registered_harness_count,
            harnesses_used_count,
            intelligence_nodes: self.db.count_intelligence_nodes().unwrap_or(0),
            cross_project_links: self
                .db
                .count_cross_project_intelligence_links()
                .unwrap_or(0),
            indexed_files: self.rag_info.indexed_files,
            rag_chunks: self.rag_info.total_chunks,
            project_count: self.projects.len() as i64,
            distinct_project_languages: languages.len(),
            gardener_edits,
            workflow_node_count_max: self.db.max_workflow_nodes_in_any_workflow().unwrap_or(0),
            completed_workflow_runs: self.db.count_completed_workflows().unwrap_or(0),
            total_workflow_node_runs: self.db.count_workflow_node_runs().unwrap_or(0),
            has_parallel_workflow: self.db.has_parallel_workflow_run().unwrap_or(false),
            seed_count: crate::domain::seeds::list_seeds()
                .map(|s| s.len())
                .unwrap_or(0),
            max_seed_session_bindings: self.db.max_seed_session_bindings().unwrap_or(0),
            agent_running,
            cpu_usage: self.system_info.cpu_usage,
            cpu_temperature_c: self.system_info.cpu_temperature,
            cpu_frequency_mhz: freq,
            max_cpu_frequency_seen: self.max_cpu_frequency_seen,
            process_count: self.system_info.process_count,
            swap_used_bytes: self.system_info.swap_used,
            gpu_vram_used: gpu.and_then(|g| g.vram_used),
            gpu_vram_total: gpu.and_then(|g| g.vram_total),
        })
    }

    fn has_active_agent_work(&self) -> bool {
        !self.active_runs.is_empty()
            || self
                .interactive_agents
                .iter()
                .any(|a| a.status == AgentStatus::Running)
            || self
                .terminal_agents
                .iter()
                .any(|a| a.status == AgentStatus::Running)
    }
}

fn registered_harness_count(config: &CanopyConfig, canopy_dir: &Path) -> usize {
    if !config.clis.is_empty() {
        return config.clis.len();
    }
    CliUsage::load(canopy_dir);
    crate::domain::models::Cli::detect_available().len()
}

fn harnesses_used_count(usage: &CliUsage, registered: usize) -> usize {
    if registered == 0 {
        return 0;
    }
    usage
        .counts
        .iter()
        .filter(|(_, &count)| count > 0)
        .count()
        .min(registered)
}
