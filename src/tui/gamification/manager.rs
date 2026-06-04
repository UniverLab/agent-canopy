//! Mission evaluation and unlock orchestration.

use std::sync::Arc;

use anyhow::Result;

use crate::db::achievements::AchievementStore;
use crate::db::Database;
use crate::domain::gamification::{mission_def, MissionId};
use crate::tui::whimsg::Whimsg;

/// One-shot events from atmosphere or user actions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MissionEvent {
    FireflyCaught,
    WorldLinked,
    IdentityEvolved,
    FirstSeedCreated,
    YoloTaskCompleted,
    DeepRagSearch,
    DigitalArcheologistFind,
}

/// Metrics gathered during `App::refresh` (cheap to collect).
#[derive(Debug, Clone, Default)]
pub struct MissionSnapshot {
    pub canopy_uptime_secs: u64,
    pub interactive_sessions: i64,
    pub registered_harness_count: usize,
    pub harnesses_used_count: usize,
    pub intelligence_nodes: i64,
    pub cross_project_links: i64,
    pub indexed_files: i64,
    pub rag_chunks: i64,
    pub project_count: i64,
    pub distinct_project_languages: usize,
    pub gardener_edits: u64,
    pub workflow_node_count_max: usize,
    pub completed_workflow_runs: i64,
    pub total_workflow_node_runs: i64,
    pub has_parallel_workflow: bool,
    pub seed_count: usize,
    pub max_seed_session_bindings: i64,
    pub agent_running: bool,
    pub cpu_usage: f32,
    pub cpu_temperature_c: Option<f32>,
    pub cpu_frequency_mhz: Option<u64>,
    pub max_cpu_frequency_seen: Option<u64>,
    pub process_count: usize,
    pub swap_used_bytes: u64,
    pub gpu_vram_used: Option<u64>,
    pub gpu_vram_total: Option<u64>,
}

pub struct MissionManager {
    store: AchievementStore,
}

impl MissionManager {
    pub fn load(db: Arc<Database>) -> Result<Self> {
        Ok(Self {
            store: AchievementStore::load(db)?,
        })
    }

    pub fn is_unlocked(&self, id: MissionId) -> bool {
        self.store.is_unlocked(id)
    }

    pub fn unlocked_count(&self) -> usize {
        self.store.unlocked_count()
    }

    pub fn process_refresh(
        &mut self,
        snapshot: &MissionSnapshot,
        events: &[MissionEvent],
        whimsg: &mut Whimsg,
    ) -> Result<Vec<String>> {
        let mut pending = Vec::new();
        let mut newly_unlocked = Vec::new();

        for event in events {
            match event {
                MissionEvent::FireflyCaught => pending.push(MissionId::FireflyCatcher),
                MissionEvent::WorldLinked => pending.push(MissionId::WorldConnector),
                MissionEvent::IdentityEvolved => pending.push(MissionId::IdentityEvolved),
                MissionEvent::FirstSeedCreated => pending.push(MissionId::FirstBloom),
                MissionEvent::YoloTaskCompleted => pending.push(MissionId::YoloPilot),
                MissionEvent::DeepRagSearch => pending.push(MissionId::DeepSearcher),
                MissionEvent::DigitalArcheologistFind => {
                    pending.push(MissionId::DigitalArcheologist);
                }
            }
        }

        Self::push_if(
            snapshot.canopy_uptime_secs >= 86_400,
            MissionId::CanopyExplorer,
            &mut pending,
        );
        Self::push_if(
            snapshot.interactive_sessions >= 5,
            MissionId::Multitasker,
            &mut pending,
        );
        Self::push_if(
            snapshot.registered_harness_count > 0
                && snapshot.harnesses_used_count >= snapshot.registered_harness_count,
            MissionId::HarnessMaster,
            &mut pending,
        );
        Self::push_if(
            snapshot.intelligence_nodes >= 100,
            MissionId::DataArchitect,
            &mut pending,
        );
        Self::push_if(
            snapshot.cross_project_links >= 1,
            MissionId::WorldConnector,
            &mut pending,
        );
        Self::push_if(
            snapshot.indexed_files >= 100,
            MissionId::UniversalLibrarian,
            &mut pending,
        );
        Self::push_if(
            snapshot.rag_chunks >= 1_000,
            MissionId::SiliconBrain,
            &mut pending,
        );
        Self::push_if(
            snapshot.project_count >= 10,
            MissionId::DataHoarder,
            &mut pending,
        );
        Self::push_if(
            snapshot.distinct_project_languages >= 3,
            MissionId::ProjectPolyglot,
            &mut pending,
        );
        Self::push_if(
            snapshot.gardener_edits >= 5,
            MissionId::TheGardener,
            &mut pending,
        );
        Self::push_if(
            snapshot.workflow_node_count_max >= 5,
            MissionId::AutomationEngineer,
            &mut pending,
        );
        Self::push_if(
            snapshot.completed_workflow_runs >= 10,
            MissionId::PipelinePilot,
            &mut pending,
        );
        Self::push_if(
            snapshot.has_parallel_workflow,
            MissionId::ParallelVision,
            &mut pending,
        );
        Self::push_if(
            snapshot.total_workflow_node_runs >= 100,
            MissionId::LoopSurvivor,
            &mut pending,
        );
        Self::push_if(
            snapshot.seed_count >= 1,
            MissionId::FirstBloom,
            &mut pending,
        );
        Self::push_if(
            snapshot.seed_count >= 5,
            MissionId::TheOrchard,
            &mut pending,
        );
        Self::push_if(
            snapshot.max_seed_session_bindings >= 50,
            MissionId::DeepRoots,
            &mut pending,
        );

        if snapshot.agent_running {
            Self::push_if(
                snapshot.cpu_usage >= 95.0,
                MissionId::FullThrottle,
                &mut pending,
            );
            if let Some(temp) = snapshot.cpu_temperature_c {
                Self::push_if(temp >= 85.0, MissionId::NuclearWinter, &mut pending);
            }
            if let (Some(used), Some(total)) = (snapshot.gpu_vram_used, snapshot.gpu_vram_total) {
                if total > 0 && used as f64 / total as f64 >= 0.9 {
                    pending.push(MissionId::VramSqueezer);
                }
            }
            Self::push_if(
                snapshot.swap_used_bytes > 1_000_000_000,
                MissionId::SwapSurvivor,
                &mut pending,
            );
            if let (Some(current), Some(max)) =
                (snapshot.cpu_frequency_mhz, snapshot.max_cpu_frequency_seen)
            {
                if current >= max && max > 0 {
                    pending.push(MissionId::Overclocker);
                }
            }
            Self::push_if(
                snapshot.process_count > 500,
                MissionId::ProcessLord,
                &mut pending,
            );
        }

        for id in pending {
            if self.store.unlock(id)? {
                let title = mission_def(id).title;
                whimsg.notify_mission_unlocked(title);
                newly_unlocked.push(title.to_string());
            }
        }

        Ok(newly_unlocked)
    }

    fn push_if(condition: bool, id: MissionId, pending: &mut Vec<MissionId>) {
        if condition {
            pending.push(id);
        }
    }
}
