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
    Workflow,
    Seeds,
    SysInfo,
}

/// Static metadata for a mission (display + persistence key).
#[derive(Debug, Clone, Copy)]
pub struct MissionDef {
    pub id: MissionId,
    pub title: &'static str,
    pub icon: &'static str,
    #[allow(dead_code)]
    pub category: MissionCategory,
}

/// Full mission registry from the gamification spec.
pub const MISSIONS: &[MissionDef] = &[
    MissionDef {
        id: MissionId::FireflyCatcher,
        title: "Cazador de Luciérnagas",
        icon: "◩",
        category: MissionCategory::Environment,
    },
    MissionDef {
        id: MissionId::HarnessMaster,
        title: "Maestro de Arneses",
        icon: "⊞",
        category: MissionCategory::Environment,
    },
    MissionDef {
        id: MissionId::CanopyExplorer,
        title: "Explorador de Canopy",
        icon: "⊚",
        category: MissionCategory::Environment,
    },
    MissionDef {
        id: MissionId::Multitasker,
        title: "Multitasking",
        icon: "⊛",
        category: MissionCategory::Environment,
    },
    MissionDef {
        id: MissionId::WorldConnector,
        title: "Conector de Mundos",
        icon: "⬙",
        category: MissionCategory::Intelligence,
    },
    MissionDef {
        id: MissionId::DataArchitect,
        title: "Arquitecto de Datos",
        icon: "◈",
        category: MissionCategory::Intelligence,
    },
    MissionDef {
        id: MissionId::UniversalLibrarian,
        title: "Bibliotecario Universal",
        icon: "◇",
        category: MissionCategory::Intelligence,
    },
    MissionDef {
        id: MissionId::SiliconBrain,
        title: "Cerebro de Silicio",
        icon: "◉",
        category: MissionCategory::Intelligence,
    },
    MissionDef {
        id: MissionId::DeepSearcher,
        title: "Buscador Profundo",
        icon: "⬢",
        category: MissionCategory::Intelligence,
    },
    MissionDef {
        id: MissionId::DigitalArcheologist,
        title: "Arqueólogo Digital",
        icon: "◓",
        category: MissionCategory::Intelligence,
    },
    MissionDef {
        id: MissionId::ProjectPolyglot,
        title: "Políglota de Proyectos",
        icon: "▣",
        category: MissionCategory::Projects,
    },
    MissionDef {
        id: MissionId::TheGardener,
        title: "El Jardinero",
        icon: "▥",
        category: MissionCategory::Projects,
    },
    MissionDef {
        id: MissionId::DataHoarder,
        title: "El Acumulador",
        icon: "▩",
        category: MissionCategory::Projects,
    },
    MissionDef {
        id: MissionId::AutomationEngineer,
        title: "Ingeniero de Automatización",
        icon: "⎔",
        category: MissionCategory::Workflow,
    },
    MissionDef {
        id: MissionId::PipelinePilot,
        title: "Piloto de Pipeline",
        icon: "◫",
        category: MissionCategory::Workflow,
    },
    MissionDef {
        id: MissionId::ParallelVision,
        title: "Visión Paralela",
        icon: "◪",
        category: MissionCategory::Workflow,
    },
    MissionDef {
        id: MissionId::LoopSurvivor,
        title: "Superviviente del Bucle",
        icon: "◯",
        category: MissionCategory::Workflow,
    },
    MissionDef {
        id: MissionId::FirstBloom,
        title: "Primer Brote",
        icon: "◰",
        category: MissionCategory::Seeds,
    },
    MissionDef {
        id: MissionId::IdentityEvolved,
        title: "Evolución Identitaria",
        icon: "◱",
        category: MissionCategory::Seeds,
    },
    MissionDef {
        id: MissionId::TheOrchard,
        title: "El Huerto",
        icon: "◲",
        category: MissionCategory::Seeds,
    },
    MissionDef {
        id: MissionId::DeepRoots,
        title: "Raíces Profundas",
        icon: "◳",
        category: MissionCategory::Seeds,
    },
    MissionDef {
        id: MissionId::FullThrottle,
        title: "A Todo Gas",
        icon: "◶",
        category: MissionCategory::SysInfo,
    },
    MissionDef {
        id: MissionId::NuclearWinter,
        title: "Invierno Nuclear",
        icon: "◷",
        category: MissionCategory::SysInfo,
    },
    MissionDef {
        id: MissionId::VramSqueezer,
        title: "VRAM Extrema",
        icon: "◴",
        category: MissionCategory::SysInfo,
    },
    MissionDef {
        id: MissionId::SwapSurvivor,
        title: "Superviviente del Swap",
        icon: "◵",
        category: MissionCategory::SysInfo,
    },
    MissionDef {
        id: MissionId::Overclocker,
        title: "Overclocker",
        icon: "◻",
        category: MissionCategory::SysInfo,
    },
    MissionDef {
        id: MissionId::ProcessLord,
        title: "Señor de Procesos",
        icon: "◼",
        category: MissionCategory::SysInfo,
    },
    MissionDef {
        id: MissionId::YoloPilot,
        title: "YOLO Master",
        icon: "◬",
        category: MissionCategory::SysInfo,
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
