//! Mission definitions from the gamification spec.

/// Stable mission identifier (persisted as `achievement:<id>`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum MissionId {
    FireflyCatcher,
    HarnessMaster,
    CanopyExplorer,
    Multitasker,
    WorldConnector,
    DataArchitect,
    UniversalLibrarian,
    SiliconBrain,
    DeepSearcher,
    DigitalArcheologist,
    ProjectPolyglot,
    TheGardener,
    DataHoarder,
    AutomationEngineer,
    PipelinePilot,
    ParallelVision,
    LoopSurvivor,
    FirstBloom,
    IdentityEvolved,
    TheOrchard,
    DeepRoots,
    FullThrottle,
    NuclearWinter,
    VramSqueezer,
    SwapSurvivor,
    Overclocker,
    ProcessLord,
    YoloPilot,
}

impl MissionId {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::FireflyCatcher => "firefly_catcher",
            Self::HarnessMaster => "harness_master",
            Self::CanopyExplorer => "canopy_explorer",
            Self::Multitasker => "multitasker",
            Self::WorldConnector => "world_connector",
            Self::DataArchitect => "data_architect",
            Self::UniversalLibrarian => "universal_librarian",
            Self::SiliconBrain => "silicon_brain",
            Self::DeepSearcher => "deep_searcher",
            Self::DigitalArcheologist => "digital_archeologist",
            Self::ProjectPolyglot => "project_polyglot",
            Self::TheGardener => "the_gardener",
            Self::DataHoarder => "data_hoarder",
            Self::AutomationEngineer => "automation_engineer",
            Self::PipelinePilot => "pipeline_pilot",
            Self::ParallelVision => "parallel_vision",
            Self::LoopSurvivor => "loop_survivor",
            Self::FirstBloom => "first_bloom",
            Self::IdentityEvolved => "identity_evolved",
            Self::TheOrchard => "the_orchard",
            Self::DeepRoots => "deep_roots",
            Self::FullThrottle => "full_throttle",
            Self::NuclearWinter => "nuclear_winter",
            Self::VramSqueezer => "vram_squeezer",
            Self::SwapSurvivor => "swap_survivor",
            Self::Overclocker => "overclocker",
            Self::ProcessLord => "process_lord",
            Self::YoloPilot => "yolo_pilot",
        }
    }

    #[allow(dead_code)]
    pub fn from_str(s: &str) -> Option<Self> {
        Some(match s {
            "firefly_catcher" => Self::FireflyCatcher,
            "harness_master" => Self::HarnessMaster,
            "canopy_explorer" => Self::CanopyExplorer,
            "multitasker" => Self::Multitasker,
            "world_connector" => Self::WorldConnector,
            "data_architect" => Self::DataArchitect,
            "universal_librarian" => Self::UniversalLibrarian,
            "silicon_brain" => Self::SiliconBrain,
            "deep_searcher" => Self::DeepSearcher,
            "digital_archeologist" => Self::DigitalArcheologist,
            "project_polyglot" => Self::ProjectPolyglot,
            "the_gardener" => Self::TheGardener,
            "data_hoarder" => Self::DataHoarder,
            "automation_engineer" => Self::AutomationEngineer,
            "pipeline_pilot" => Self::PipelinePilot,
            "parallel_vision" => Self::ParallelVision,
            "loop_survivor" => Self::LoopSurvivor,
            "first_bloom" => Self::FirstBloom,
            "identity_evolved" => Self::IdentityEvolved,
            "the_orchard" => Self::TheOrchard,
            "deep_roots" => Self::DeepRoots,
            "full_throttle" => Self::FullThrottle,
            "nuclear_winter" => Self::NuclearWinter,
            "vram_squeezer" => Self::VramSqueezer,
            "swap_survivor" => Self::SwapSurvivor,
            "overclocker" => Self::Overclocker,
            "process_lord" => Self::ProcessLord,
            "yolo_pilot" => Self::YoloPilot,
            _ => return None,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MissionCategory {
    Environment,
    Intelligence,
    Projects,
    Loop,
    Seeds,
    SysInfo,
}

/// Static metadata for a mission (display + persistence key).
#[derive(Debug, Clone, Copy)]
pub struct MissionDef {
    pub id: MissionId,
    pub title: &'static str,
    pub icon: &'static str,
    pub category: MissionCategory,
    pub challenge: &'static str,
}

/// Full mission registry from the gamification spec.
pub const MISSIONS: &[MissionDef] = &[
    MissionDef {
        id: MissionId::FireflyCatcher,
        title: "Firefly Catcher",
        icon: "◩",
        category: MissionCategory::Environment,
        challenge: "Catch a firefly by clicking on it during Night Scene",
    },
    MissionDef {
        id: MissionId::HarnessMaster,
        title: "Harness Master",
        icon: "⊞",
        category: MissionCategory::Environment,
        challenge: "Use every registered CLI harness at least once",
    },
    MissionDef {
        id: MissionId::CanopyExplorer,
        title: "Canopy Explorer",
        icon: "⊚",
        category: MissionCategory::Environment,
        challenge: "Accumulate 24 hours of total Canopy uptime",
    },
    MissionDef {
        id: MissionId::Multitasker,
        title: "Multitasker",
        icon: "⊛",
        category: MissionCategory::Environment,
        challenge: "Have 5 or more active interactive sessions at once",
    },
    MissionDef {
        id: MissionId::WorldConnector,
        title: "World Connector",
        icon: "⬙",
        category: MissionCategory::Intelligence,
        challenge: "Link two different project nodes in the Intelligence Graph",
    },
    MissionDef {
        id: MissionId::DataArchitect,
        title: "Data Architect",
        icon: "◈",
        category: MissionCategory::Intelligence,
        challenge: "Reach 100+ nodes in the global Intelligence Graph",
    },
    MissionDef {
        id: MissionId::UniversalLibrarian,
        title: "Universal Librarian",
        icon: "◇",
        category: MissionCategory::Intelligence,
        challenge: "Index 100+ files in the global RAG store",
    },
    MissionDef {
        id: MissionId::SiliconBrain,
        title: "Silicon Brain",
        icon: "◉",
        category: MissionCategory::Intelligence,
        challenge: "Reach 1,000+ chunks in the global vector database",
    },
    MissionDef {
        id: MissionId::DeepSearcher,
        title: "Deep Searcher",
        icon: "⬢",
        category: MissionCategory::Intelligence,
        challenge: "Perform a RAG search with relevance distance < 0.2",
    },
    MissionDef {
        id: MissionId::DigitalArcheologist,
        title: "Digital Archeologist",
        icon: "◓",
        category: MissionCategory::Intelligence,
        challenge: "Find a RAG match from a session older than 1 month",
    },
    MissionDef {
        id: MissionId::ProjectPolyglot,
        title: "Project Polyglot",
        icon: "▣",
        category: MissionCategory::Projects,
        challenge: "Have projects in 3+ different programming languages",
    },
    MissionDef {
        id: MissionId::TheGardener,
        title: "The Gardener",
        icon: "▥",
        category: MissionCategory::Projects,
        challenge: "Update descriptions or tags for 5+ different projects",
    },
    MissionDef {
        id: MissionId::DataHoarder,
        title: "Data Hoarder",
        icon: "▩",
        category: MissionCategory::Projects,
        challenge: "Have 10+ projects indexed in the global Canopy system",
    },
    MissionDef {
        id: MissionId::AutomationEngineer,
        title: "Automation Engineer",
        icon: "⎔",
        category: MissionCategory::Loop,
        challenge: "Run a loop with 5 or more interconnected nodes",
    },
    MissionDef {
        id: MissionId::PipelinePilot,
        title: "Pipeline Pilot",
        icon: "◫",
        category: MissionCategory::Loop,
        challenge: "Complete 10 full loop executions",
    },
    MissionDef {
        id: MissionId::ParallelVision,
        title: "Parallel Vision",
        icon: "◪",
        category: MissionCategory::Loop,
        challenge: "Run a loop with parallelizable specs enabled",
    },
    MissionDef {
        id: MissionId::LoopSurvivor,
        title: "Loop Survivor",
        icon: "◯",
        category: MissionCategory::Loop,
        challenge: "Execute 100+ individual loop node runs",
    },
    MissionDef {
        id: MissionId::FirstBloom,
        title: "First Bloom",
        icon: "◰",
        category: MissionCategory::Seeds,
        challenge: "Create your first Seed Identity via the Nursery loop",
    },
    MissionDef {
        id: MissionId::IdentityEvolved,
        title: "Identity Evolved",
        icon: "◱",
        category: MissionCategory::Seeds,
        challenge: "Use evolve_identity to refine a Seed's traits or directives",
    },
    MissionDef {
        id: MissionId::TheOrchard,
        title: "The Orchard",
        icon: "◲",
        category: MissionCategory::Seeds,
        challenge: "Have 5 or more active Seed Identities in your registry",
    },
    MissionDef {
        id: MissionId::DeepRoots,
        title: "Deep Roots",
        icon: "◳",
        category: MissionCategory::Seeds,
        challenge: "A single Seed participates in 50+ different sessions",
    },
    MissionDef {
        id: MissionId::FullThrottle,
        title: "Full Throttle",
        icon: "◶",
        category: MissionCategory::SysInfo,
        challenge: "Reach 95%+ CPU usage while an agent is running",
    },
    MissionDef {
        id: MissionId::NuclearWinter,
        title: "Nuclear Winter",
        icon: "◷",
        category: MissionCategory::SysInfo,
        challenge: "CPU temperature exceeds 85°C (185°F)",
    },
    MissionDef {
        id: MissionId::VramSqueezer,
        title: "VRAM Squeezer",
        icon: "◴",
        category: MissionCategory::SysInfo,
        challenge: "GPU VRAM usage exceeds 90%",
    },
    MissionDef {
        id: MissionId::SwapSurvivor,
        title: "Swap Survivor",
        icon: "◵",
        category: MissionCategory::SysInfo,
        challenge: "System enters Swap memory usage > 1GB",
    },
    MissionDef {
        id: MissionId::Overclocker,
        title: "Overclocker",
        icon: "◻",
        category: MissionCategory::SysInfo,
        challenge: "CPU frequency reaches its maximum detected value",
    },
    MissionDef {
        id: MissionId::ProcessLord,
        title: "Process Lord",
        icon: "◼",
        category: MissionCategory::SysInfo,
        challenge: "System process count exceeds 500 while Canopy is active",
    },
    MissionDef {
        id: MissionId::YoloPilot,
        title: "YOLO Pilot",
        icon: "◬",
        category: MissionCategory::SysInfo,
        challenge: "Complete a task using an agent in YOLO mode",
    },
];

pub fn mission_def(id: MissionId) -> &'static MissionDef {
    MISSIONS
        .iter()
        .find(|m| m.id == id)
        .expect("every MissionId must have a MissionDef")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn all_mission_ids_have_defs() {
        assert_eq!(MISSIONS.len(), 28);
        for def in MISSIONS {
            assert_eq!(mission_def(def.id).id, def.id);
            assert_eq!(MissionId::from_str(def.id.as_str()), Some(def.id));
        }
    }
}
